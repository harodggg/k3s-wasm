# wasm 镜像的最小写法：只有一个模块文件。
#
# 用法（wasmtime shim 路径）：
#   cargo build --release --target wasm32-wasip2
#   docker build -t k3s-wasm/hello-http:dev .
#   docker save k3s-wasm/hello-http:dev | k3s ctr images import -
#   kubectl run hello --image=k3s-wasm/hello-http:dev \
#       --overrides='{"spec":{"runtimeClassName":"wasmtime-wasip2"}}'
#
# 如果 shim 报找不到 wasm 模块，就改用 Docker 的 wasm 平台构建：
#   docker buildx build --platform wasi/wasm -t k3s-wasm/hello-http:dev .
FROM scratch
COPY target/wasm32-wasip2/release/hello_http.wasm /hello.wasm
ENTRYPOINT ["/hello.wasm"]
