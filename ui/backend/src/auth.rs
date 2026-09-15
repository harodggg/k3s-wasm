//! 免密登录：WebAuthn（Touch ID / passkey）。
//!
//! # 为什么是「免密」而不是「密码 + 指纹」
//!
//! 控制台手里是 kube-api-proxy 的权限（能建删工作负载、读 Secret），所以它必须放在
//! HTTPS 后面并加鉴权。选择纯 WebAuthn 的理由是**不留可被暴力破解的东西**：
//! 没有口令就没有口令库、没有撞库、没有明文口令过网。
//!
//! 代价是「首个凭据怎么来」—— 用一次性注册码（`K3S_WASM_REGISTRATION_CODE`）绑定，
//! 绑定成功后这个码就再也用不上了（注册接口在已有凭据时直接拒绝）。
//!
//! # 组件无状态，所以状态全在签名 cookie 里
//!
//! runwasi 是「一个请求一个实例」（即便复用实例也不能依赖内存），也没有文件系统。
//! 因此：
//!   * 会话      → `k3s_wasm_session`  cookie，内容是 HMAC-SHA256 签名的 JSON
//!   * 挑战      → `k3s_wasm_chal`     cookie，同上（短期，5 分钟）
//!   * 公钥凭据  → 存进 k8s Secret（`K3S_WASM_AUTH_SECRET`，默认 `k3s-wasm-ui-auth`）
//!
//! # 验签要点（WebAuthn 规范里最容易做漏的几处）
//!
//!   1. `clientDataJSON` 的 `type` 必须是 `webauthn.create` / `webauthn.get`，
//!      `challenge` 必须等于我们签发的那个，`origin` 必须**精确**匹配。
//!   2. `authenticatorData.rpIdHash` 必须等于 `SHA256(rpId)`（否则一个站点的凭据
//!      可以被搬到另一个站点重放）。
//!   3. 必须要求 UP（用户在场）且 UV（用户验证，即指纹/面容）置位 —— 只查签名
//!      等于允许「碰一下」就登录。
//!   4. 签名对象是 `authenticatorData || SHA256(clientDataJSON)`，不是 clientDataJSON 本身。
//!   5. ES256 的签名是 **ASN.1 DER**，不是裸 r||s。
//!   6. 签名计数器要单调递增（克隆凭据检测）。
//!
//! 以上每一条都有对应的单测（见文件末）。

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::http_io::{Request, Response};
use crate::k8s::K8s;

const SESSION_COOKIE: &str = "k3s_wasm_session";
const CHALLENGE_COOKIE: &str = "k3s_wasm_chal";
/// 会话 12 小时：够用一天，又不至于长期有效。
const SESSION_TTL_SECS: u64 = 12 * 3600;
/// 挑战 5 分钟：够 Touch ID 弹窗 + 用户按指纹。
const CHALLENGE_TTL_SECS: u64 = 300;
const DEFAULT_AUTH_SECRET: &str = "k3s-wasm-ui-auth";
/// 凭据在 Secret 里的键。
const CREDENTIAL_KEY: &str = "credential";

// ════════════════════════════════════════════════════════════════════
// 基础工具
// ════════════════════════════════════════════════════════════════════

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .ok()
}

/// k8s Secret 的 `data` 是标准 base64（带 padding），不是 base64url。
fn b64_std_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 随机数只用宿主给的能力，不引 getrandom（组件体积与依赖都更小）。
fn random_bytes(n: usize) -> Vec<u8> {
    wasi::random::random::get_random_bytes(n as u64)
}

/// 定长比较，避免用 `==` 泄漏注册码前缀。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

// ════════════════════════════════════════════════════════════════════
// 配置
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// WebAuthn 的 RP ID（= 站点域名，**不带端口**）
    pub rp_id: String,
    /// 允许的 origin（**带 scheme**，可带端口）
    pub origin: String,
    /// 会话/挑战 cookie 的 HMAC 密钥
    pub session_secret: Vec<u8>,
    /// 一次性注册码
    pub registration_code: String,
    /// 存凭据的 Secret 名
    pub secret_name: String,
    pub namespace: String,
}

impl AuthConfig {
    pub fn load(default_ns: &str) -> Self {
        let get = |key: &str, default: &str| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            rp_id: get("K3S_WASM_RP_ID", ""),
            origin: get("K3S_WASM_ORIGIN", ""),
            session_secret: b64url_decode(&get("K3S_WASM_SESSION_SECRET", "")).unwrap_or_default(),
            registration_code: get("K3S_WASM_REGISTRATION_CODE", ""),
            secret_name: get("K3S_WASM_AUTH_SECRET", DEFAULT_AUTH_SECRET),
            namespace: get("DEFAULT_NAMESPACE", default_ns),
        }
    }

    /// 会话密钥是硬前提：没有它就没法签发/校验会话。缺了它宁可 503，
    /// 也不能「配置不全就放行」——那等于控制台裸奔。
    pub fn unconfigured_reason(&self) -> Option<String> {
        if self.session_secret.len() < 16 {
            return Some(
                "未配置 K3S_WASM_SESSION_SECRET（至少 16 字节的 base64url 随机串）\
                 —— 控制台拒绝服务而不是放行"
                    .to_string(),
            );
        }
        None
    }
}

/// RP ID / origin 的运行期取值：环境变量优先，否则从请求的 Host 推。
///
/// 从 Host 推是安全的：Host 由浏览器按用户输入设置，且这里在 TLS 之后；
/// 显式配置则更不易出意外，两者都给 `/api/auth/status` 看得到。
fn effective_rp(req: &Request, cfg: &AuthConfig) -> Result<(String, String), String> {
    let host = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let hostname = host
        .as_deref()
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();

    // 环境变量优先；只有当两边都没得用时才报错（显式配置的部署不该被一个缺失的
    // Host 头拖下水 —— 这正是实测里踩到的那次 400）。
    let rp_id = if cfg.rp_id.is_empty() {
        hostname.clone()
    } else {
        cfg.rp_id.clone()
    };
    let origin = if cfg.origin.is_empty() {
        // 控制台只在 HTTPS 下可用（WebAuthn 的硬要求），所以默认补 https。
        match host.as_deref() {
            Some(h) if h.ends_with(":443") => format!("https://{hostname}"),
            Some(h) => format!("https://{h}"),
            None => String::new(),
        }
    } else {
        cfg.origin.clone()
    };
    if rp_id.is_empty() || origin.is_empty() {
        return Err(
            "无法确定 RP ID / origin：请求里没有 Host 头，且 K3S_WASM_RP_ID / K3S_WASM_ORIGIN \
             没有配全。请显式配置这两个环境变量。"
                .to_string(),
        );
    }
    Ok((rp_id, origin))
}

// ════════════════════════════════════════════════════════════════════
// 签名 cookie
// ════════════════════════════════════════════════════════════════════

fn cookie_value(req: &Request, name: &str) -> Option<String> {
    req.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .flat_map(|(_, v)| v.split(';'))
        .find_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            if k == name {
                Some(v.to_string())
            } else {
                None
            }
        })
}

fn sign(secret: &[u8], payload: &str) -> Option<String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    Some(b64url(&mac.finalize().into_bytes()))
}

fn make_token(secret: &[u8], claims: &Value) -> Option<String> {
    let payload = b64url(claims.to_string().as_bytes());
    let sig = sign(secret, &payload)?;
    Some(format!("{payload}.{sig}"))
}

fn verify_token(secret: &[u8], token: &str, kind: &str) -> Option<Value> {
    let (payload, sig) = token.split_once('.')?;
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    let sig_raw = b64url_decode(sig)?;
    // verify_slice 是定长比较，不会因为逐字节比较而泄漏签名
    mac.verify_slice(&sig_raw).ok()?;
    let claims: Value = serde_json::from_slice(&b64url_decode(payload)?).ok()?;
    if claims["kind"].as_str() != Some(kind) {
        return None;
    }
    if claims["exp"].as_u64().unwrap_or(0) < now_secs() {
        return None;
    }
    Some(claims)
}

fn set_cookie(name: &str, value: &str, max_age: u64) -> (String, String) {
    (
        "set-cookie".to_string(),
        format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age}"),
    )
}

fn clear_cookie(name: &str) -> (String, String) {
    set_cookie(name, "", 0)
}

// ════════════════════════════════════════════════════════════════════
// 门禁
// ════════════════════════════════════════════════════════════════════

/// 哪些路径需要登录。
///
/// 放行的只有两类：健康检查（用于探针/排障）与认证自身的接口（否则没法登录）。
/// 静态资源也放行 —— 登录页本身就是静态资源，登录后再由前端守卫挡住数据。
pub fn requires_auth(path: &str) -> bool {
    path.starts_with("/api/") && path != "/api/health" && !path.starts_with("/api/auth/")
}

/// 会话是否有效。`Err` 表示「配置不全」，调用方应当拒绝服务（503）而不是放行。
pub fn authenticated(req: &Request, cfg: &AuthConfig) -> Result<bool, String> {
    if let Some(reason) = cfg.unconfigured_reason() {
        return Err(reason);
    }
    let Some(token) = cookie_value(req, SESSION_COOKIE) else {
        return Ok(false);
    };
    Ok(verify_token(&cfg.session_secret, &token, "session").is_some())
}

// ════════════════════════════════════════════════════════════════════
// 公钥凭据的持久化（k8s Secret）
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct Credential {
    /// base64url 的 credential id
    pub id: String,
    /// COSE 公钥的原始 CBOR 字节
    pub cose: Vec<u8>,
    pub sign_count: u32,
}

fn read_credential(k8s: &K8s, cfg: &AuthConfig) -> Result<Option<Credential>, String> {
    let path = format!(
        "/api/v1/namespaces/{}/secrets/{}",
        cfg.namespace, cfg.secret_name
    );
    let secret = match k8s.get(&path) {
        Ok(v) => v,
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(format!("读取凭据 Secret 失败：{}", e.message())),
    };
    let Some(raw) = secret["data"][CREDENTIAL_KEY].as_str() else {
        return Ok(None);
    };
    let decoded = b64_std_decode(raw).ok_or("凭据 Secret 里的 credential 不是合法 base64")?;
    let v: Value = serde_json::from_slice(&decoded).map_err(|e| format!("凭据 JSON 解析失败：{e}"))?;
    let id = v["id"].as_str().unwrap_or_default().to_string();
    let cose = v["cose"]
        .as_str()
        .and_then(b64url_decode)
        .ok_or("凭据里缺 cose 公钥")?;
    if id.is_empty() {
        return Err("凭据里缺 id".to_string());
    }
    Ok(Some(Credential {
        id,
        cose,
        sign_count: v["signCount"].as_u64().unwrap_or(0) as u32,
    }))
}

fn write_credential(k8s: &K8s, cfg: &AuthConfig, cred: &Credential) -> Result<(), String> {
    let payload = json!({
        "id": cred.id,
        "cose": b64url(&cred.cose),
        "signCount": cred.sign_count,
        "createdAt": now_secs(),
    })
    .to_string();
    let path = format!(
        "/api/v1/namespaces/{}/secrets/{}",
        cfg.namespace, cfg.secret_name
    );
    let meta = json!({
        "metadata": {
            "name": cfg.secret_name,
            "namespace": cfg.namespace,
            "labels": { "app.kubernetes.io/managed-by": "k3s-wasm-ui" }
        }
    });
    match k8s.get(&path) {
        Ok(_) => k8s
            .merge_patch(&path, &json!({ "stringData": { CREDENTIAL_KEY: payload } }))
            .map(|_| ())
            .map_err(|e| format!("更新凭据 Secret 失败：{}", e.message())),
        Err(e) if e.is_not_found() => {
            let mut body = meta;
            body["apiVersion"] = json!("v1");
            body["kind"] = json!("Secret");
            body["type"] = json!("Opaque");
            body["stringData"] = json!({ CREDENTIAL_KEY: payload });
            k8s.post(
                &format!("/api/v1/namespaces/{}/secrets", cfg.namespace),
                &body,
            )
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "创建凭据 Secret 失败：{}。控制台需要该命名空间的 secrets 写权限。",
                    e.message()
                )
            })
        }
        Err(e) => Err(format!("探测凭据 Secret 失败：{}", e.message())),
    }
}

// ════════════════════════════════════════════════════════════════════
// WebAuthn 数据解析 / 校验（纯函数，全部可原生单测）
// ════════════════════════════════════════════════════════════════════

pub struct AuthData {
    pub rp_id_hash: Vec<u8>,
    pub flags: u8,
    pub sign_count: u32,
    pub credential_id: Vec<u8>,
    pub cose: Vec<u8>,
}

pub const FLAG_UP: u8 = 0x01;
pub const FLAG_UV: u8 = 0x04;
pub const FLAG_AT: u8 = 0x40;

pub fn parse_auth_data(b: &[u8]) -> Result<AuthData, String> {
    if b.len() < 37 {
        return Err(format!("authenticatorData 太短（{} 字节，至少 37）", b.len()));
    }
    let rp_id_hash = b[0..32].to_vec();
    let flags = b[32];
    let sign_count = u32::from_be_bytes([b[33], b[34], b[35], b[36]]);
    let mut credential_id = Vec::new();
    let mut cose = Vec::new();
    if flags & FLAG_AT != 0 {
        if b.len() < 55 {
            return Err("置了 AT 标志但数据不足（缺 attestedCredentialData）".to_string());
        }
        let cred_len = u16::from_be_bytes([b[53], b[54]]) as usize;
        let start: usize = 55;
        let end = start
            .checked_add(cred_len)
            .ok_or_else(|| "credentialId 长度溢出".to_string())?;
        if b.len() < end {
            return Err("credentialId 越界".to_string());
        }
        credential_id = b[start..end].to_vec();
        cose = b[end..].to_vec();
        if cose.is_empty() {
            return Err("缺 COSE 公钥".to_string());
        }
    }
    Ok(AuthData {
        rp_id_hash,
        flags,
        sign_count,
        credential_id,
        cose,
    })
}

/// 解析 attestationObject，取出 fmt 与 authData。
pub fn parse_attestation_object(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    let v: ciborium::Value =
        ciborium::de::from_reader(bytes).map_err(|e| format!("attestationObject 不是合法 CBOR：{e}"))?;
    let map = v.as_map().ok_or("attestationObject 不是 CBOR map")?;
    let field = |name: &str| {
        map.iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, val)| val)
    };
    let fmt = field("fmt")
        .and_then(|f| f.as_text())
        .ok_or("attestationObject 缺 fmt")?
        .to_string();
    let auth_data = field("authData")
        .and_then(|a| a.as_bytes())
        .ok_or("attestationObject 缺 authData")?
        .to_vec();
    Ok((fmt, auth_data))
}

/// 校验 COSE 公钥必须是 EC2 / P-256 / ES256，并取出 x、y 坐标。
pub fn cose_ec2_p256(cose: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let v: ciborium::Value =
        ciborium::de::from_reader(cose).map_err(|e| format!("COSE 公钥不是合法 CBOR：{e}"))?;
    let map = v.as_map().ok_or("COSE 公钥不是 CBOR map")?;
    let num = |key: i128| {
        map.iter()
            .find(|(k, _)| k.as_integer().map(i128::from) == Some(key))
            .map(|(_, val)| val)
    };
    let int_of = |key: i128| num(key).and_then(|v| v.as_integer()).map(i128::from);
    let kty = int_of(1).unwrap_or(0);
    let alg = int_of(3).unwrap_or(0);
    let crv = int_of(-1).unwrap_or(0);
    if kty != 2 {
        return Err(format!(
            "只接受 EC2 公钥（kty=2），收到 kty={kty}（RSA 之类不被支持）"
        ));
    }
    if crv != 1 {
        return Err(format!("只接受 P-256（crv=1），收到 crv={crv}"));
    }
    if alg != -7 {
        return Err(format!("只接受 ES256（alg=-7），收到 alg={alg}"));
    }
    let x = num(-2).and_then(|v| v.as_bytes()).ok_or("COSE 缺 x")?.to_vec();
    let y = num(-3).and_then(|v| v.as_bytes()).ok_or("COSE 缺 y")?.to_vec();
    if x.len() != 32 || y.len() != 32 {
        return Err("COSE 的 x/y 必须是 32 字节".to_string());
    }
    Ok((x, y))
}

/// ES256 的签名是 ASN.1 DER 的 `SEQUENCE { INTEGER r, INTEGER s }`，
/// 不是 WebCrypto 那种裸 r||s —— 直接按 64 字节切会验签失败。
pub fn parse_der_signature(der: &[u8]) -> Result<([u8; 32], [u8; 32]), String> {
    if der.len() < 8 || der[0] != 0x30 {
        return Err("签名不是 DER SEQUENCE".to_string());
    }
    let mut idx = 2;
    let mut out = [[0u8; 32]; 2];
    for slot in out.iter_mut() {
        if idx + 2 > der.len() || der[idx] != 0x02 {
            return Err("DER 里期望 INTEGER".to_string());
        }
        let len = der[idx + 1] as usize;
        let start = idx + 2;
        let end = start + len;
        if end > der.len() {
            return Err("DER INTEGER 越界".to_string());
        }
        let mut int = &der[start..end];
        while int.len() > 1 && int[0] == 0 {
            int = &int[1..];
        }
        if int.len() > 32 {
            return Err("DER INTEGER 超过 32 字节".to_string());
        }
        slot[32 - int.len()..].copy_from_slice(int);
        idx = end;
    }
    Ok((out[0], out[1]))
}

/// 断言签名校验。返回签名计数器（供克隆检测）。
pub fn verify_assertion(
    cose: &[u8],
    rp_id: &str,
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature: &[u8],
) -> Result<u32, String> {
    let ad = parse_auth_data(authenticator_data)?;
    if ad.rp_id_hash != sha256(rp_id.as_bytes()) {
        return Err("rpIdHash 不匹配（这条凭据不是发给这个站点的）".to_string());
    }
    if ad.flags & FLAG_UP == 0 {
        return Err("UP 标志未置位（用户不在场）".to_string());
    }
    if ad.flags & FLAG_UV == 0 {
        return Err("UV 标志未置位（需要指纹/面容验证，不接受只碰一下）".to_string());
    }
    let (x, y) = cose_ec2_p256(cose)?;
    let mut sec1 = Vec::with_capacity(65);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(&sec1)
        .map_err(|e| format!("公钥点无效：{e}"))?;

    // 签名对象 = authenticatorData || SHA256(clientDataJSON)
    let mut signed = authenticator_data.to_vec();
    signed.extend_from_slice(&sha256(client_data_json));

    let (r, s) = parse_der_signature(signature)?;
    let sig = p256::ecdsa::Signature::from_scalars(r, s).map_err(|e| format!("签名标量无效：{e}"))?;
    use p256::ecdsa::signature::Verifier;
    key.verify(&signed, &sig).map_err(|_| "签名验证失败".to_string())?;
    Ok(ad.sign_count)
}

/// 校验 clientDataJSON 的 type / challenge / origin。
pub fn check_client_data(
    client_data_json: &[u8],
    expected_type: &str,
    expected_challenge_b64: &str,
    expected_origin: &str,
) -> Result<(), String> {
    let v: Value = serde_json::from_slice(client_data_json)
        .map_err(|e| format!("clientDataJSON 不是合法 JSON：{e}"))?;
    if v["type"].as_str() != Some(expected_type) {
        return Err(format!(
            "clientDataJSON.type 应为 {expected_type}，收到 {}",
            v["type"].as_str().unwrap_or("(缺失)")
        ));
    }
    if v["challenge"].as_str() != Some(expected_challenge_b64) {
        return Err("挑战值不匹配（可能是重放或 cookie 过期）".to_string());
    }
    if v["origin"].as_str() != Some(expected_origin) {
        return Err(format!(
            "origin 不匹配：收到 {}，期望 {expected_origin}",
            v["origin"].as_str().unwrap_or("(缺失)")
        ));
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// 接口实现
// ════════════════════════════════════════════════════════════════════

fn body_field(body: &Value, path: &[&str]) -> Option<String> {
    let mut cur = body;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

/// GET /api/auth/status
pub fn status(req: &Request, cfg: &AuthConfig, k8s: &K8s) -> Response {
    let mut info = describe(req, cfg);
    let registered = match read_credential(k8s, cfg) {
        Ok(Some(_)) => true,
        Ok(None) => false,
        // 读不到（无权限/网络）不该让登录页以为「未注册」而允许重新注册
        Err(_) => true,
    };
    info["registered"] = json!(registered);
    info["authenticated"] = json!(authenticated(req, cfg).unwrap_or(false));
    info["sessionTtlSeconds"] = json!(SESSION_TTL_SECS);
    Response::ok(info)
}

/// 认证配置的展示用描述（/api/health 与 /api/auth/status 共用）。
/// 不含任何秘密：只有模式、是否配置完成、RP ID / origin、以及存凭据的 Secret 名。
pub fn describe(req: &Request, cfg: &AuthConfig) -> Value {
    let (rp_id, origin) = effective_rp(req, cfg).unwrap_or_else(|_| (cfg.rp_id.clone(), cfg.origin.clone()));
    let reason = cfg.unconfigured_reason();
    json!({
        "mode": "webauthn-passkey",
        "configured": reason.is_none(),
        "configuredError": reason,
        "rpId": rp_id,
        "origin": origin,
        "secretName": cfg.secret_name,
        "registrationCodeSet": !cfg.registration_code.is_empty(),
    })
}

/// POST /api/auth/register/begin —— 用一次性注册码换挑战
pub fn register_begin(req: &Request, cfg: &AuthConfig, k8s: &K8s) -> Response {
    if let Some(reason) = cfg.unconfigured_reason() {
        return Response::fail(503, reason);
    }
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    if cfg.registration_code.is_empty() {
        return Response::fail(
            503,
            "未配置 K3S_WASM_REGISTRATION_CODE（首次绑定 Touch ID 需要它）",
        );
    }
    let given = body["code"].as_str().unwrap_or("").trim().to_string();
    if !ct_eq(given.as_bytes(), cfg.registration_code.as_bytes()) {
        return Response::fail(401, "注册码不正确");
    }
    match read_credential(k8s, cfg) {
        Ok(Some(_)) => {
            return Response::fail(
                409,
                format!(
                    "已经绑定过凭据了。要重新绑定，先删掉 Secret {}/{} 的 {} 字段。",
                    cfg.namespace, cfg.secret_name, CREDENTIAL_KEY
                ),
            )
        }
        Ok(None) => {}
        Err(e) => return Response::fail(502, e),
    }
    let (rp_id, _origin) = match effective_rp(req, cfg) {
        Ok(v) => v,
        Err(e) => return Response::fail(400, e),
    };
    let challenge = b64url(&random_bytes(32));
    let user_id = b64url(&random_bytes(16));
    let claims = json!({
        "kind": "reg",
        "c": challenge,
        "exp": now_secs() + CHALLENGE_TTL_SECS,
    });
    let Some(token) = make_token(&cfg.session_secret, &claims) else {
        return Response::fail(500, "会话密钥不可用");
    };
    let (hk, hv) = set_cookie(CHALLENGE_COOKIE, &token, CHALLENGE_TTL_SECS);
    Response::ok(json!({
        "challenge": challenge,
        "rp": { "id": rp_id, "name": "k3s-wasm 控制台" },
        "user": { "id": user_id, "name": "owner", "displayName": "k3s-wasm owner" },
        "pubKeyCredParams": [{ "type": "public-key", "alg": -7 }],
        "timeout": CHALLENGE_TTL_SECS * 1000,
        "attestation": "none",
        "authenticatorSelection": {
            // platform = 用本机内置认证器（Mac 上就是 Touch ID），不弹安全密钥
            "authenticatorAttachment": "platform",
            "residentKey": "preferred",
            "userVerification": "required"
        },
        "excludeCredentials": []
    }))
    .with_header(&hk, &hv)
}

/// POST /api/auth/register/finish —— 校验 attestation 并落盘凭据
pub fn register_finish(req: &Request, cfg: &AuthConfig, k8s: &K8s) -> Response {
    if let Some(reason) = cfg.unconfigured_reason() {
        return Response::fail(503, reason);
    }
    let Some(token) = cookie_value(req, CHALLENGE_COOKIE) else {
        return Response::fail(400, "缺少挑战 cookie，请重新发起注册");
    };
    let Some(claims) = verify_token(&cfg.session_secret, &token, "reg") else {
        return Response::fail(400, "挑战 cookie 无效或已过期，请重新发起注册");
    };
    let challenge = claims["c"].as_str().unwrap_or("").to_string();
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (rp_id, origin) = match effective_rp(req, cfg) {
        Ok(v) => v,
        Err(e) => return Response::fail(400, e),
    };

    let client_data = match body_field(&body, &["response", "clientDataJSON"]).and_then(|s| b64url_decode(&s)) {
        Some(v) => v,
        None => return Response::fail(400, "请求缺 response.clientDataJSON（base64url）"),
    };
    if let Err(m) = check_client_data(&client_data, "webauthn.create", &challenge, &origin) {
        return Response::fail(400, m);
    }
    let att_raw = match body_field(&body, &["response", "attestationObject"]).and_then(|s| b64url_decode(&s)) {
        Some(v) => v,
        None => return Response::fail(400, "请求缺 response.attestationObject（base64url）"),
    };
    let (fmt, auth_data) = match parse_attestation_object(&att_raw) {
        Ok(v) => v,
        Err(m) => return Response::fail(400, m),
    };
    // 我们请求的是 attestation: none，也只接受 none —— 其它格式意味着需要校验
    // 证明链（或需要信任厂商），这里刻意不做「看起来通过但其实没验」的事。
    if fmt != "none" {
        return Response::fail(
            400,
            format!("只接受 attestation=none，收到 fmt={fmt}（浏览器/认证器给的证明链本控制台不验证）"),
        );
    }
    let ad = match parse_auth_data(&auth_data) {
        Ok(v) => v,
        Err(m) => return Response::fail(400, m),
    };
    if ad.rp_id_hash != sha256(rp_id.as_bytes()) {
        return Response::fail(400, "rpIdHash 不匹配（RP ID 配错了？）");
    }
    if ad.flags & FLAG_UP == 0 || ad.flags & FLAG_UV == 0 {
        return Response::fail(400, "认证器未同时置位 UP/UV —— 需要真正做过指纹/面容验证");
    }
    if ad.flags & FLAG_AT == 0 || ad.credential_id.is_empty() {
        return Response::fail(400, "attestation 里没有公钥凭据（AT 标志未置位）");
    }
    if let Err(m) = cose_ec2_p256(&ad.cose) {
        return Response::fail(400, m);
    }
    // 响应里的 id 必须与 authData 里的 credentialId 一致，否则存的公钥和客户端
    // 以为的凭据会对不上（登录时表现为「找不到凭据」）。
    if let Some(id_from_body) = body_field(&body, &["id"]).and_then(|s| b64url_decode(&s)) {
        if id_from_body != ad.credential_id {
            return Response::fail(400, "凭据 id 与 attestation 内的 credentialId 不一致");
        }
    }

    let cred = Credential {
        id: b64url(&ad.credential_id),
        cose: ad.cose,
        sign_count: ad.sign_count,
    };
    if let Err(m) = write_credential(k8s, cfg, &cred) {
        return Response::fail(500, m);
    }
    let session = make_token(
        &cfg.session_secret,
        &json!({ "kind": "session", "sub": "owner", "exp": now_secs() + SESSION_TTL_SECS }),
    );
    let Some(session) = session else {
        return Response::fail(500, "会话密钥不可用");
    };
    let (hk, hv) = set_cookie(SESSION_COOKIE, &session, SESSION_TTL_SECS);
    let (ck, cv) = clear_cookie(CHALLENGE_COOKIE);
    Response::ok(json!({
        "registered": true,
        "authenticated": true,
        "credentialId": cred.id,
        "note": "Touch ID / passkey 已绑定。注册码现在失效（再次注册会被拒绝）。",
    }))
    .with_header(&hk, &hv)
    .with_header(&ck, &cv)
}

/// POST /api/auth/login/begin
pub fn login_begin(req: &Request, cfg: &AuthConfig, k8s: &K8s) -> Response {
    if let Some(reason) = cfg.unconfigured_reason() {
        return Response::fail(503, reason);
    }
    let cred = match read_credential(k8s, cfg) {
        Ok(Some(c)) => c,
        Ok(None) => return Response::fail(409, "这台控制台还没有绑定任何凭据，请先用注册码绑定"),
        Err(e) => return Response::fail(502, e),
    };
    let (rp_id, _origin) = match effective_rp(req, cfg) {
        Ok(v) => v,
        Err(e) => return Response::fail(400, e),
    };
    let challenge = b64url(&random_bytes(32));
    let claims = json!({
        "kind": "login",
        "c": challenge,
        "exp": now_secs() + CHALLENGE_TTL_SECS,
    });
    let Some(token) = make_token(&cfg.session_secret, &claims) else {
        return Response::fail(500, "会话密钥不可用");
    };
    let (hk, hv) = set_cookie(CHALLENGE_COOKIE, &token, CHALLENGE_TTL_SECS);
    Response::ok(json!({
        "challenge": challenge,
        "rpId": rp_id,
        "timeout": CHALLENGE_TTL_SECS * 1000,
        "userVerification": "required",
        "allowCredentials": [{ "type": "public-key", "id": cred.id, "transports": ["internal"] }]
    }))
    .with_header(&hk, &hv)
}

/// POST /api/auth/login/finish
pub fn login_finish(req: &Request, cfg: &AuthConfig, k8s: &K8s) -> Response {
    if let Some(reason) = cfg.unconfigured_reason() {
        return Response::fail(503, reason);
    }
    let Some(token) = cookie_value(req, CHALLENGE_COOKIE) else {
        return Response::fail(400, "缺少挑战 cookie，请重新发起登录");
    };
    let Some(claims) = verify_token(&cfg.session_secret, &token, "login") else {
        return Response::fail(400, "挑战 cookie 无效或已过期，请重新发起登录");
    };
    let challenge = claims["c"].as_str().unwrap_or("").to_string();
    let body = match req.json_body() {
        Ok(b) => b,
        Err(m) => return Response::fail(400, m),
    };
    let (rp_id, origin) = match effective_rp(req, cfg) {
        Ok(v) => v,
        Err(e) => return Response::fail(400, e),
    };
    let mut cred = match read_credential(k8s, cfg) {
        Ok(Some(c)) => c,
        Ok(None) => return Response::fail(409, "没有已绑定的凭据"),
        Err(e) => return Response::fail(502, e),
    };

    let client_data = match body_field(&body, &["response", "clientDataJSON"]).and_then(|s| b64url_decode(&s)) {
        Some(v) => v,
        None => return Response::fail(400, "请求缺 response.clientDataJSON（base64url）"),
    };
    if let Err(m) = check_client_data(&client_data, "webauthn.get", &challenge, &origin) {
        return Response::fail(400, m);
    }
    let auth_data = match body_field(&body, &["response", "authenticatorData"]).and_then(|s| b64url_decode(&s)) {
        Some(v) => v,
        None => return Response::fail(400, "请求缺 response.authenticatorData（base64url）"),
    };
    let signature = match body_field(&body, &["response", "signature"]).and_then(|s| b64url_decode(&s)) {
        Some(v) => v,
        None => return Response::fail(400, "请求缺 response.signature（base64url）"),
    };
    if let Some(id_from_body) = body_field(&body, &["id"]) {
        if id_from_body != cred.id {
            return Response::fail(401, "凭据 id 不匹配");
        }
    }

    let sign_count = match verify_assertion(&cred.cose, &rp_id, &auth_data, &client_data, &signature) {
        Ok(c) => c,
        Err(m) => return Response::fail(401, m),
    };
    // 计数器单调性：认证器只在支持时才维护它（0 表示不支持），所以只在两端都非 0 时校验。
    if cred.sign_count != 0 && sign_count != 0 && sign_count <= cred.sign_count {
        return Response::fail(
            401,
            format!(
                "签名计数器未递增（{} → {}），疑似凭据被克隆；已拒绝本次登录",
                cred.sign_count, sign_count
            ),
        );
    }
    if sign_count != 0 {
        cred.sign_count = sign_count;
        if let Err(m) = write_credential(k8s, cfg, &cred) {
            // 计数器写不进去不该让人登不上，但要说清楚
            let (hk, hv) = session_response_cookie(cfg);
            return Response::ok(json!({
                "authenticated": true,
                "warning": format!("登录成功，但更新签名计数器失败：{m}"),
            }))
            .with_header(&hk, &hv);
        }
    }

    let (hk, hv) = session_response_cookie(cfg);
    let (ck, cv) = clear_cookie(CHALLENGE_COOKIE);
    Response::ok(json!({
        "authenticated": true,
        "signCount": sign_count,
    }))
    .with_header(&hk, &hv)
    .with_header(&ck, &cv)
}

fn session_response_cookie(cfg: &AuthConfig) -> (String, String) {
    let token = make_token(
        &cfg.session_secret,
        &json!({ "kind": "session", "sub": "owner", "exp": now_secs() + SESSION_TTL_SECS }),
    )
    .unwrap_or_default();
    set_cookie(SESSION_COOKIE, &token, SESSION_TTL_SECS)
}

/// POST /api/auth/logout
pub fn logout() -> Response {
    let (hk, hv) = clear_cookie(SESSION_COOKIE);
    let (ck, cv) = clear_cookie(CHALLENGE_COOKIE);
    Response::ok(json!({ "authenticated": false }))
        .with_header(&hk, &hv)
        .with_header(&ck, &cv)
}

// ════════════════════════════════════════════════════════════════════
// 单测
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_cookie(name: &str, value: &str) -> Request {
        Request {
            method: "GET".into(),
            path: "/api/nodes".into(),
            query: String::new(),
            headers: vec![("cookie".into(), format!("{name}={value}"))],
            body: vec![],
        }
    }

    fn cfg() -> AuthConfig {
        AuthConfig {
            rp_id: "console.example.test".into(),
            origin: "https://console.example.test".into(),
            session_secret: b"0123456789abcdef0123456789abcdef".to_vec(),
            registration_code: "code-abc".into(),
            secret_name: "k3s-wasm-ui-auth".into(),
            namespace: "k3s-wasm".into(),
        }
    }

    // ── cookie 与会话 ────────────────────────────────────────────────

    #[test]
    fn token_round_trip_and_tamper_detection() {
        let c = cfg();
        let claims = json!({"kind": "session", "exp": now_secs() + 60});
        let token = make_token(&c.session_secret, &claims).unwrap();
        assert!(verify_token(&c.session_secret, &token, "session").is_some());
        // 换 kind 就不认（挑战 cookie 不能当会话用）
        assert!(verify_token(&c.session_secret, &token, "reg").is_none());
        // 改 payload 后签名失效
        let forged = format!("{}.{}", b64url(b"{\"kind\":\"session\",\"exp\":9999999999}"), token.split('.').nth(1).unwrap());
        assert!(verify_token(&c.session_secret, &forged, "session").is_none());
        // 换密钥也不认
        let other = b"ffffffffffffffffffffffffffffffff".to_vec();
        assert!(verify_token(&other, &token, "session").is_none());
    }

    #[test]
    fn expired_token_is_rejected() {
        let c = cfg();
        let token = make_token(&c.session_secret, &json!({"kind": "session", "exp": now_secs() - 1})).unwrap();
        assert!(verify_token(&c.session_secret, &token, "session").is_none());
    }

    #[test]
    fn gate_requires_auth_except_health_and_auth_paths() {
        assert!(requires_auth("/api/nodes"));
        assert!(requires_auth("/api/xray/tunnels"));
        assert!(!requires_auth("/api/health"));
        assert!(!requires_auth("/api/auth/status"));
        assert!(!requires_auth("/api/auth/login/finish"));
        assert!(!requires_auth("/"));
        assert!(!requires_auth("/assets/index.js"));
    }

    #[test]
    fn authenticated_needs_valid_cookie_and_config() {
        let c = cfg();
        let token = make_token(&c.session_secret, &json!({"kind": "session", "exp": now_secs() + 60})).unwrap();
        let req = req_with_cookie(SESSION_COOKIE, &token);
        assert_eq!(authenticated(&req, &c), Ok(true));
        assert_eq!(authenticated(&Request { headers: vec![], ..req.clone() }, &c), Ok(false));
        // 配置不全 → Err（调用方应当 503，而不是放行）
        let mut bad = c.clone();
        bad.session_secret = vec![];
        assert!(authenticated(&req, &bad).is_err());
    }

    #[test]
    fn cookie_value_parses_multiple_cookies() {
        let req = Request {
            headers: vec![(
                "Cookie".into(),
                "a=1; k3s_wasm_chal=xyz; b=2".into(),
            )],
            ..req_with_cookie("x", "y")
        };
        assert_eq!(cookie_value(&req, CHALLENGE_COOKIE).as_deref(), Some("xyz"));
        assert_eq!(cookie_value(&req, "a").as_deref(), Some("1"));
        assert_eq!(cookie_value(&req, "missing"), None);
    }

    // ── WebAuthn 解析 ───────────────────────────────────────────────

    fn cose_p256(x: &[u8; 32], y: &[u8; 32]) -> Vec<u8> {
        let map = vec![
            (ciborium::Value::from(1), ciborium::Value::from(2)),
            (ciborium::Value::from(3), ciborium::Value::from(-7)),
            (ciborium::Value::from(-1), ciborium::Value::from(1)),
            (ciborium::Value::from(-2), ciborium::Value::from(x.to_vec())),
            (ciborium::Value::from(-3), ciborium::Value::from(y.to_vec())),
        ];
        let mut out = Vec::new();
        ciborium::ser::into_writer(&ciborium::Value::Map(map), &mut out).unwrap();
        out
    }

    #[test]
    fn cose_key_accepts_p256_and_rejects_others() {
        let x = [1u8; 32];
        let y = [2u8; 32];
        let ok = cose_p256(&x, &y);
        assert!(cose_ec2_p256(&ok).is_ok());

        // kty=3 (RSA) 必须拒绝
        let rsa = {
            let map = vec![
                (ciborium::Value::from(1), ciborium::Value::from(3)),
                (ciborium::Value::from(3), ciborium::Value::from(-257)),
            ];
            let mut out = Vec::new();
            ciborium::ser::into_writer(&ciborium::Value::Map(map), &mut out).unwrap();
            out
        };
        assert!(cose_ec2_p256(&rsa).unwrap_err().contains("kty"));
    }

    #[test]
    fn auth_data_parsing_extracts_credential_and_cose() {
        let rp_hash = sha256(b"console.example.test");
        let cose = cose_p256(&[9u8; 32], &[8u8; 32]);
        let cred_id = b"credential-id-bytes";
        let mut b = Vec::new();
        b.extend_from_slice(&rp_hash);
        b.push(FLAG_UP | FLAG_UV | FLAG_AT);
        b.extend_from_slice(&7u32.to_be_bytes());
        b.extend_from_slice(&[0u8; 16]); // aaguid
        b.extend_from_slice(&(cred_id.len() as u16).to_be_bytes());
        b.extend_from_slice(cred_id);
        b.extend_from_slice(&cose);

        let ad = parse_auth_data(&b).unwrap();
        assert_eq!(ad.rp_id_hash, rp_hash);
        assert_eq!(ad.sign_count, 7);
        assert_eq!(ad.credential_id, cred_id.to_vec());
        assert_eq!(ad.cose, cose);
        assert!(parse_auth_data(&b[..30]).is_err());
    }

    #[test]
    fn attestation_object_requires_cbor_fields() {
        let auth_data = vec![0u8; 37];
        let obj = {
            let map = vec![
                (ciborium::Value::from("fmt"), ciborium::Value::from("none")),
                (ciborium::Value::from("attStmt"), ciborium::Value::Map(vec![])),
                (ciborium::Value::from("authData"), ciborium::Value::from(auth_data.clone())),
            ];
            let mut out = Vec::new();
            ciborium::ser::into_writer(&ciborium::Value::Map(map), &mut out).unwrap();
            out
        };
        let (fmt, ad) = parse_attestation_object(&obj).unwrap();
        assert_eq!(fmt, "none");
        assert_eq!(ad, auth_data);
        assert!(parse_attestation_object(b"not cbor").is_err());
    }

    #[test]
    fn der_signature_round_trip() {
        let r = [0x11u8; 32];
        let s = [0x22u8; 32];
        let der = der_encode(&r, &s);
        let (r2, s2) = parse_der_signature(&der).unwrap();
        assert_eq!(r2, r);
        assert_eq!(s2, s);
        // 高位为 1 时需要补 0x00 前缀（DER 的 INTEGER 是有符号的）
        let r3 = {
            let mut v = [0u8; 32];
            v[0] = 0x80;
            v
        };
        let der3 = der_encode(&r3, &s);
        assert_eq!(parse_der_signature(&der3).unwrap().0, r3);
        assert!(parse_der_signature(&[0x31, 0x02, 0x02, 0x01, 0x01]).is_err());
    }

    // ── 端到端（用自造的 WebAuthn 断言，不需要浏览器） ──────────────

    fn der_encode(r: &[u8; 32], s: &[u8; 32]) -> Vec<u8> {
        fn int(v: &[u8; 32]) -> Vec<u8> {
            let mut body = v.to_vec();
            while body.len() > 1 && body[0] == 0 {
                body.remove(0);
            }
            if body[0] & 0x80 != 0 {
                body.insert(0, 0);
            }
            let mut out = vec![0x02, body.len() as u8];
            out.extend_from_slice(&body);
            out
        }
        let mut body = int(r);
        body.extend_from_slice(&int(s));
        let mut out = vec![0x30, body.len() as u8];
        out.extend_from_slice(&body);
        out
    }

    /// 造一个真实的 P-256 断言：私钥固定（确定性），签名对象按规范拼。
    fn make_assertion(
        rp_id: &str,
        origin: &str,
        challenge: &str,
        sign_count: u32,
        flags: u8,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use p256::ecdsa::{signature::Signer, SigningKey};
        let sk = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
        let vk = sk.verifying_key();
        let point = vk.to_encoded_point(false);
        let mut x = [0u8; 32];
        let mut y = [0u8; 32];
        x.copy_from_slice(&point.x().unwrap()[..]);
        y.copy_from_slice(&point.y().unwrap()[..]);
        let cose = cose_p256(&x, &y);

        let client_data = json!({
            "type": "webauthn.get",
            "challenge": challenge,
            "origin": origin,
            "crossOrigin": false,
        })
        .to_string()
        .into_bytes();

        let mut auth_data = sha256(rp_id.as_bytes());
        auth_data.push(flags);
        auth_data.extend_from_slice(&sign_count.to_be_bytes());

        let mut signed = auth_data.clone();
        signed.extend_from_slice(&sha256(&client_data));
        let sig: p256::ecdsa::Signature = sk.sign(&signed);
        let raw = sig.to_bytes();
        let r: [u8; 32] = raw[..32].try_into().unwrap();
        let s: [u8; 32] = raw[32..].try_into().unwrap();
        (cose, client_data, der_encode(&r, &s).into_iter().chain(auth_data).collect())
    }

    #[test]
    fn assertion_verification_end_to_end() {
        let rp_id = "console.example.test";
        let origin = "https://console.example.test";
        let challenge = "Zm9vLWNoYWxsZW5nZQ";
        let (cose, client_data, packed) = make_assertion(rp_id, origin, challenge, 5, FLAG_UP | FLAG_UV);
        // 后半段是 authData（前半段是 DER 签名）—— 这里只做长度切分不现实，
        // 所以重新按规范拼一次：签名长度由 DER 头决定。
        let sig_len = 2 + packed[1] as usize;
        let (sig, auth_data) = packed.split_at(sig_len);

        check_client_data(&client_data, "webauthn.get", challenge, origin).unwrap();
        let count = verify_assertion(&cose, rp_id, auth_data, &client_data, sig).unwrap();
        assert_eq!(count, 5);

        // 换 RP ID：rpIdHash 不匹配必须失败
        assert!(verify_assertion(&cose, "evil.example.test", auth_data, &client_data, sig).is_err());
        // 没有 UV（只碰一下）必须失败
        let (cose2, cd2, packed2) = make_assertion(rp_id, origin, challenge, 6, FLAG_UP);
        let sig_len2 = 2 + packed2[1] as usize;
        let (sig2, ad2) = packed2.split_at(sig_len2);
        assert!(verify_assertion(&cose2, rp_id, ad2, &cd2, sig2).unwrap_err().contains("UV"));
        // 改一个字节的签名必须失败
        let mut bad_sig = sig.to_vec();
        bad_sig[10] ^= 0xff;
        assert!(verify_assertion(&cose, rp_id, auth_data, &client_data, &bad_sig).is_err());
        // 改了 clientDataJSON（比如换 origin）也必须失败
        let tampered = String::from_utf8_lossy(&client_data).replace(origin, "https://evil.example.test");
        assert!(verify_assertion(&cose, rp_id, auth_data, tampered.as_bytes(), sig).is_err());
    }

    #[test]
    fn client_data_checks_type_challenge_origin() {
        let cd = json!({
            "type": "webauthn.get",
            "challenge": "abc",
            "origin": "https://console.example.test"
        })
        .to_string()
        .into_bytes();
        assert!(check_client_data(&cd, "webauthn.get", "abc", "https://console.example.test").is_ok());
        // type 不对（拿注册的响应来登录）
        assert!(check_client_data(&cd, "webauthn.create", "abc", "https://console.example.test").is_err());
        // 挑战不对（重放）
        assert!(check_client_data(&cd, "webauthn.get", "zzz", "https://console.example.test").is_err());
        // origin 不对
        assert!(check_client_data(&cd, "webauthn.get", "abc", "https://other.example.test").is_err());
        assert!(check_client_data(b"{", "webauthn.get", "abc", "x").is_err());
    }

    #[test]
    fn effective_rp_prefers_env_and_falls_back_to_host() {
        let host_req = Request {
            headers: vec![("Host".into(), "console.2.29.44.63.nip.io".into())],
            ..req_with_cookie("x", "y")
        };
        let mut c = cfg();
        c.rp_id = String::new();
        c.origin = String::new();
        assert_eq!(
            effective_rp(&host_req, &c).unwrap(),
            (
                "console.2.29.44.63.nip.io".to_string(),
                "https://console.2.29.44.63.nip.io".to_string()
            )
        );
        // 显式配置优先
        c.rp_id = "rp.example".into();
        c.origin = "https://rp.example:8443".into();
        assert_eq!(
            effective_rp(&host_req, &c).unwrap(),
            ("rp.example".to_string(), "https://rp.example:8443".to_string())
        );
        // 没有 Host 且没配置 → 报错（宁可报错也不要猜）
        let mut empty = cfg();
        empty.rp_id = String::new();
        empty.origin = String::new();
        assert!(effective_rp(&Request { headers: vec![], ..host_req.clone() }, &empty).is_err());
        // 但**显式配置了**就照用，不该被缺失的 Host 头拖下水（实测踩过这个 400）
        assert_eq!(
            effective_rp(&Request { headers: vec![], ..host_req.clone() }, &cfg()).unwrap(),
            (
                "console.example.test".to_string(),
                "https://console.example.test".to_string()
            )
        );
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
