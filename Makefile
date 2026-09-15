# k3s-wasm —— 常用操作入口。
#
# 目标分成两类，别混：
#   「本机能做」的：构建、单测、本地端到端（mock k8s + 真 wasm 宿主）
#   「必须在节点上做」的：装 shim、部署、集群验证

SHELL := /usr/bin/env bash
REPO  := $(shell cd "$(dir $(lastword $(MAKEFILE_LIST)))" && pwd)

# rustup 的 cargo 必须在 PATH 最前：Homebrew 的 rustc 没有 wasm32-wasip2 sysroot，
# 用它编 wasm 会报 "can't find crate for core"，看不出真正原因。
export PATH := $(HOME)/.cargo/bin:$(PATH)
export CARGO_HOME ?= $(REPO)/../.cargo
export npm_config_cache ?= $(REPO)/../.npm-cache

.DEFAULT_GOAL := help

.PHONY: help
help: ## 显示所有目标
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-22s\033[0m %s\n", $$1, $$2}'

# ── 本机 ────────────────────────────────────────────────────────────

.PHONY: ui
ui: ## 构建前端 + wasm 后端
	./scripts/build-ui.sh

.PHONY: frontend
frontend: ## 只构建前端
	cd ui/frontend && npm install --no-audit --no-fund && npm run build

.PHONY: wasm
wasm: ## 只构建 wasm 后端
	cd ui/backend && cargo build --release --target wasm32-wasip2
	@ls -la ui/backend/target/wasm32-wasip2/release/k3s_wasm_ui.wasm

.PHONY: test
test: ## 原生单测（29 个：路由、JSON 形态、参数校验、URL/query 解析）
	cd ui/backend && cargo test

.PHONY: e2e
e2e: ## 本地端到端：mock k8s API + 真实 wasm 宿主（spin 或 wasmtime）+ 31 项断言
	./scripts/e2e-local-test.sh

.PHONY: dev
dev: ## 本地起控制台（带 mock k8s API），浏览器打开 http://127.0.0.1:8080
	./scripts/dev-serve-local.sh

.PHONY: fmt
fmt: ## 格式化
	cd ui/backend && cargo fmt
	cd ui/frontend && npx --yes prettier --write 'src/**/*.ts' 2>/dev/null || true

.PHONY: example
example: ## 构建两个示例（hello-http / hello-wasip2）
	cd examples/hello-http && cargo build --release --target wasm32-wasip2
	cd examples/hello-wasip2 && cargo build --release --target wasm32-wasip2
	@echo "产物："
	@ls -la examples/*/target/wasm32-wasip2/release/*.wasm

.PHONY: clean
clean: ## 清理构建产物（保留依赖缓存）
	rm -rf ui/frontend/dist ui/frontend/node_modules
	rm -rf ui/backend/target examples/*/target
	rm -rf .wasmtime-cache

# ── 节点 / 集群（多数需要 root 或集群权限）──────────────────────────

.PHONY: runtime
runtime: ## [节点] 装 wasm 运行时（spin + wasmtime shim），需要 sudo
	sudo ./scripts/install-wasm-runtime.sh

.PHONY: runtime-dry
runtime-dry: ## [节点] 装之前先看要改什么
	sudo ./scripts/install-wasm-runtime.sh --dry-run

.PHONY: spinkube
spinkube: ## [集群] 装 SpinKube（cert-manager + RCM + operator + CRD）
	sudo ./scripts/install-spinkube.sh --no-runtime-class-manager

.PHONY: image
image: ## [本机] 构建镜像并导入 k3s containerd（无需仓库）
	./scripts/build-ui.sh --import

.PHONY: deploy
deploy: ## [集群] 部署控制台（默认路径：wasmtime shim + Deployment）
	kubectl apply -k deploy/overlays/shim-only

.PHONY: deploy-spinkube
deploy-spinkube: ## [集群] 以 SpinApp 形式部署（需先 make spinkube）
	kubectl apply -k deploy/overlays/spinkube

.PHONY: verify
verify: ## [集群] 端到端验证 wasm 运行时（真跑一个 wasm 工作负载）
	./scripts/verify-wasm-runtime.sh

.PHONY: render
render: ## 渲染清单（不 apply），用于检查
	@echo '── shim-only ──'; kubectl kustomize deploy/overlays/shim-only
	@echo '── spinkube ──';  kubectl kustomize deploy/overlays/spinkube
