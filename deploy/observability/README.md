# minicoding 可观测性基础设施

一键启动 OpenTelemetry 观测后端（collector + Jaeger + Prometheus + Grafana），供
minicoding 各入口（CLI / server / 桌面 sidecar）上报 trace 与 metrics。设计详见
`docs/observability.md` §7。

## 快速开始

```bash
cd deploy/observability
cp .env.example .env   # 可选：按需修改端口/镜像版本
docker compose up -d
```

启动后组件与端口（全部仅绑定 `127.0.0.1`）：

| 组件 | 端口 | 用途 |
|------|------|------|
| otel-collector | 4318 (HTTP) / 4317 (gRPC) | minicoding 上报端点（OTLP） |
| Jaeger | 16686 | trace 查询 UI |
| Prometheus | 9090 | metrics 查询 |
| Grafana | 3000 | 统一 UI（admin/admin，首次登录后修改） |

停止：`docker compose down`（加 `-v` 清空 Prometheus/Grafana 数据卷）。

## 接入 minicoding

minicoding 通过标准 OTel 环境变量接入，**无需改任何配置文件**：

```bash
# CLI
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 minicoding --verbose

# Server（桌面端 sidecar 同此，宿主环境变量被继承）
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 minicoding-server --bind 127.0.0.1:8080
```

可选：`OTEL_TRACES_SAMPLER=traceidratio` 按比例采样（`OTEL_TRACES_SAMPLER_ARG=0.1`）。

## 查看数据

1. **Trace**：Jaeger `http://127.0.0.1:16686`，选 service `minicoding-server`（或 `minicoding`），
   主要 span：`session` / `turn` / `llm.chat_stream` / `tool.call` / `permission.check`
2. **Metrics**：Grafana `http://127.0.0.1:3000` → Explore → Prometheus（datasource 已预置）

## 架构

```text
minicoding (宿主) ──OTLP:4318──▶ otel-collector ──gRPC──▶ jaeger (trace, UI:16686)
                                        │──/metrics:8889──▶ prometheus (9090)
                                        └────────────────────▶ grafana (3000, 预置 datasource)
```

## 生产注意事项

- Jaeger all-in-one 默认**内存存储**，重启丢数据；生产换持久存储
- 默认仅本机可访问（绑定 `127.0.0.1`）；请勿暴露到局域网
- Grafana 默认密码 `admin/admin`，通过 `GRAFANA_ADMIN_PASSWORD` 修改
- 上报数据不含 API key（minicoding C-04 约束，凭证不落 span/日志）
- 镜像版本在 `.env` 中可覆盖（见 `.env.example`）
