#!/usr/bin/env python3
"""make-wasm-image.py —— 不依赖 docker，把一个 .wasm 打成可被 k3s/containerd 导入的 OCI 镜像。

为什么需要它：
  · 构建机上常常没有 docker 守护进程（CI、受限环境、只有 ctr 的机器）；
  · wasm 镜像有两种流派，而 runwasi shim 对它们的接受度不一样，文档里也没说清：
      --layout rootfs     —— 普通 linux 镜像，层里放 /app.wasm，Entrypoint 指向它。
                             平台是 linux/<arch>，kubelet 不会有平台抱怨；
                             命令式组件（wasi:cli/run）靠这个路径找模块。
      --layout wasm-layer —— 层就是**裸 wasm**，mediaType 用 application/wasm，
                             平台标成 wasi/wasm。这是 runwasi 文档里那种镜像。
    两种都提供，是因为「哪种能跑」要靠实测（本仓库在真节点上试过并记录了结论）。

用法：
  ./scripts/make-wasm-image.py --wasm app.wasm --tag myapp:dev --out myapp.tar
  ./scripts/make-wasm-image.py --wasm app.wasm --tag myapp:dev --layout wasm-layer --out myapp.tar
  # 送到节点
  k3s ctr images import myapp.tar
"""
from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import sys
import tarfile
import tempfile
from pathlib import Path

OCI_LAYOUT_VERSION = "1.0.0"
MT_INDEX = "application/vnd.oci.image.index.v1+json"
MT_MANIFEST = "application/vnd.oci.image.manifest.v1+json"
MT_CONFIG = "application/vnd.oci.image.config.v1+json"
MT_LAYER_TAR = "application/vnd.oci.image.layer.v1.tar+gzip"
MT_LAYER_WASM = "application/wasm"
MT_COMPONENT_WASM = "application/vnd.bytecodealliance.wasm.component.layer.v0+wasm"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Blobs:
    """OCI 的 blob 存储（内存里攒，最后一次性写进 tar）"""

    def __init__(self) -> None:
        self.items: dict[str, bytes] = {}

    def add(self, data: bytes) -> tuple[str, int]:
        digest = sha256(data)
        self.items[digest] = data
        return f"sha256:{digest}", len(data)


def build_rootfs_layer(wasm_name: str, wasm: bytes) -> tuple[bytes, str]:
    """把 wasm 放进 gzip 过的 tar 层，返回 (压缩后的层字节, 解压后 tar 的 digest)。

    ⚠️ 这里有个必须分清的点：OCI 里
      · 层描述符的 digest = **压缩后** blob 的 sha256
      · rootfs.diff_ids   = **解压后** tar 的 sha256
    两者混用会被 containerd 直接拒绝：
      wrong diff id "sha256:..." calculated on extraction "sha256:..."
    """
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as tf:
        info = tarfile.TarInfo(name=wasm_name.lstrip("/"))
        info.size = len(wasm)
        info.mode = 0o644
        info.mtime = 0
        tf.addfile(info, io.BytesIO(wasm))
    raw_bytes = raw.getvalue()
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb", mtime=0) as gz:
        gz.write(raw_bytes)
    return buf.getvalue(), f"sha256:{sha256(raw_bytes)}"


def make_image(wasm_path: Path, tag: str, layout: str, entrypoint: str, arch: str) -> bytes:
    wasm = wasm_path.read_bytes()
    repo, _, tagpart = tag.partition(":")
    tagpart = tagpart or "latest"
    wasm_in_image = os.path.basename(wasm_path)

    blobs = Blobs()

    if layout == "rootfs":
        layer_bytes, diff_id = build_rootfs_layer(wasm_in_image, wasm)
        layer_media = MT_LAYER_TAR
        platform = {"architecture": arch, "os": "linux"}
        cfg = {
            "Entrypoint": [f"/{wasm_in_image}"],
            "Env": ["PATH=/"],
        }
    else:
        # 裸 wasm 作为层：层内容即模块，未压缩，所以 diff_id == 层 digest
        layer_bytes = wasm
        layer_media = MT_LAYER_WASM
        platform = {"architecture": "wasm", "os": "wasi"}
        cfg = {"Entrypoint": [f"/{wasm_in_image}"]}

    layer_digest, layer_size = blobs.add(layer_bytes)

    config = {
        "architecture": platform["architecture"],
        "os": platform["os"],
        "config": cfg,
        "rootfs": {"type": "layers", "diff_ids": [diff_id]},
    }
    config_bytes = json.dumps(config, separators=(",", ":")).encode()
    config_digest, config_size = blobs.add(config_bytes)

    manifest = {
        "schemaVersion": 2,
        "mediaType": MT_MANIFEST,
        "config": {"mediaType": MT_CONFIG, "digest": config_digest, "size": config_size},
        # 带 annotations 让 shim 知道原始文件名（部分实现会用它找模块）
        "layers": [
            {
                "mediaType": layer_media,
                "digest": layer_digest,
                "size": layer_size,
                "annotations": {"org.opencontainers.image.title": wasm_in_image},
            }
        ],
        "annotations": {"org.opencontainers.image.title": repo},
    }
    manifest_bytes = json.dumps(manifest, separators=(",", ":")).encode()
    manifest_digest, manifest_size = blobs.add(manifest_bytes)

    index = {
        "schemaVersion": 2,
        "mediaType": MT_INDEX,
        "manifests": [
            {
                "mediaType": MT_MANIFEST,
                "digest": manifest_digest,
                "size": manifest_size,
                "platform": platform,
                # 这里必须是完整引用 repo:tag；只写 tag 的话 ctr 会把它当成
                # 一个乱七八糟的默认名（实测得到过 "import 2026 09 15:dev" 这种名字）
                "annotations": {"org.opencontainers.image.ref.name": tag},
            }
        ],
    }
    index_bytes = json.dumps(index, separators=(",", ":")).encode()

    # 打成 OCI layout tar（ctr images import 认这个）
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w") as tf:
        def add_file(name: str, data: bytes) -> None:
            info = tarfile.TarInfo(name=name)
            info.size = len(data)
            info.mode = 0o644
            info.mtime = 0
            tf.addfile(info, io.BytesIO(data))

        add_file("oci-layout", json.dumps({"imageLayoutVersion": OCI_LAYOUT_VERSION}).encode())
        add_file("index.json", index_bytes)
        for digest, data in blobs.items.items():
            add_file(f"blobs/sha256/{digest}", data)
    return out.getvalue()


def main() -> int:
    ap = argparse.ArgumentParser(description="把 .wasm 打成 OCI 镜像（不需要 docker）")
    ap.add_argument("--wasm", required=True, type=Path, help="输入的 wasm 文件")
    ap.add_argument("--tag", required=True, help="镜像标签，如 k3s-wasm/hello:dev")
    ap.add_argument("--out", required=True, type=Path, help="输出的 OCI layout tar")
    ap.add_argument("--layout", choices=["rootfs", "wasm-layer"], default="rootfs",
                    help="rootfs=普通 linux 镜像里放 .wasm（默认）；wasm-layer=裸 wasm 层 + wasi/wasm 平台")
    ap.add_argument("--arch", default="amd64", help="rootfs 布局下的 CPU 架构（默认 amd64）")
    args = ap.parse_args()

    if not args.wasm.is_file():
        print(f"找不到 {args.wasm}", file=sys.stderr)
        return 1

    data = make_image(args.wasm, args.tag, args.layout, "/", args.arch)
    args.out.write_bytes(data)
    print(f"✓ {args.out}  ({len(data)} bytes)")
    print(f"  tag    : {args.tag}")
    print(f"  layout : {args.layout}"
          + ("" if args.layout == "rootfs" else f"  (mediaType {MT_LAYER_WASM})"))
    print(f"  导入   : k3s ctr -n k8s.io images import {args.out}   # kubelet 只看 k8s.io 命名空间")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
