---
title: Observability
description: The opt-in logs/metrics/traces stack — Vector, OpenObserve, and OpenTelemetry.
---

Observability is **opt-in** — enable it with `observability.enabled: true` and dabba adds the
stack to the cluster's Flux selection.

## What it deploys

| Component | Role |
|-----------|------|
| [Vector](https://vector.dev) (DaemonSet) | Collects every pod's logs and ships them to the backend. The sink is where you change backends. |
| [OpenTelemetry Collector](https://opentelemetry.io/docs/collector/) | Receives OTLP traces/metrics from apps (`otel-collector.observability.svc:4317`) and forwards them to the backend. |
| [OpenObserve](https://openobserve.ai) | Self-hosted, single-binary backend + UI for logs/metrics/traces, at `https://o2.<domain>`. |

Log in with `admin@dabba.local` and the per-env password from `dabba secret get dabba/openobserve`.

## Pluggable backends

The backend is a swap point — Vector's sink and the OTEL exporter. OpenObserve is the convenient
self-hosted default; SaaS backends (Honeycomb, Datadog) and other self-hosted options slot in by
repointing the sink/exporter.

## Hardening for production

The stack ships with local defaults: storage is `emptyDir` (logs and traces are lost when a pod
restarts), each component is a single replica, and the workloads carry no resource limits or
probes. Add persistence, limits, and replicas before relying on it beyond a laptop.
