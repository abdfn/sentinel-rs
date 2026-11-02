# syntax=docker/dockerfile:1
# sentinel-rs 容器化构建
#   wreq/btls 依赖 BoringSSL 现场编译：cmake+ninja+clang/libclang(bindgen)+perl+go
#   Go 工具链直接从本地已有的 golang:1.27.3-bookworm 镜像里抽，不走 apt(golang-go 1.19 太老)

FROM golang:1.27.3-bookworm AS gobin

# ── 构建阶段 ────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake ninja-build clang libclang-dev perl pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY --from=gobin /usr/local/go /usr/local/go
ENV PATH=/usr/local/go/bin:$PATH \
    LIBCLANG_PATH=/usr/lib/llvm-14/lib

WORKDIR /usr/src/app
COPY . .

# 构建 + 跑离线测试（README 保证测试不联网）
# target/ 与 cargo registry 挂 BuildKit 缓存：改源码后重建只重编本项目，不重编 BoringSSL
RUN --mount=type=cache,target=/usr/src/app/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked \
    && cargo test --release --locked \
    && mkdir -p /out \
    && cp target/release/sentinel-rs target/release/tls_probe target/release/token_check \
          target/release/so_check target/release/profile_mint target/release/sentinel-trace /out/

# ── 运行阶段 ────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /out/ /usr/local/bin/

COPY sample_challenge.json tools/mock_mail.py ./

ENV SENTINEL_DB_PATH=/data/sentinel-rs.db
VOLUME ["/data"]

ENTRYPOINT ["sentinel-rs"]
CMD ["--help"]
