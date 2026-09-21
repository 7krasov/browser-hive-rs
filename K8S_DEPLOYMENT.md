# Kubernetes Deployment Guide

This document describes the Kubernetes deployment configuration required for Browser Hive with graceful shutdown support.

## Required Configurations

### Worker Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: browser-hive-worker
spec:
  replicas: 3
  selector:
    matchLabels:
      app: browser-hive-worker
      scope: your-scope-name  # Match your scope
  template:
    metadata:
      labels:
        app: browser-hive-worker
        scope: your-scope-name
    spec:
      terminationGracePeriodSeconds: 60  # Time between SIGTERM and SIGKILL
      containers:
      - name: worker
        image: browser-hive-worker:latest
        ports:
        - containerPort: 50052
          name: grpc
          protocol: TCP
        - containerPort: 9090
          name: metrics
          protocol: TCP

        # TCP, not `grpc:` — a K8s gRPC probe calls the standard grpc.health.v1.Health
        # service, which neither binary implements, so it would never pass. The gRPC port
        # opens only after the browser pool is up, so "port open" means "started".
        # NOTE: this removes the pod from the Service endpoints, which the coordinator does
        # not use — it discovers pods and polls HealthCheck itself (see "Health Check
        # Behavior"). It gates rollouts, it does not drain.
        readinessProbe:
          tcpSocket:
            port: 50052
          initialDelaySeconds: 5
          periodSeconds: 2
          timeoutSeconds: 3
          successThreshold: 1
          failureThreshold: 2

        # Restarts a worker whose gRPC server is gone. It cannot see a wedged browser.
        livenessProbe:
          tcpSocket:
            port: 50052
          initialDelaySeconds: 60   # browser launch + WORKER_MIN_CONTEXTS before the port opens
          periodSeconds: 10
          timeoutSeconds: 5
          failureThreshold: 3

        # Runs BEFORE SIGTERM: during the sleep the worker is still healthy and still
        # routed to. Only useful for Service consumers; the coordinator stops routing on
        # the first HealthCheck after SIGTERM. See GRACEFUL_SHUTDOWN.md.
        lifecycle:
          preStop:
            exec:
              command: ["/bin/sh", "-c", "sleep 5"]

        env:
        - name: WORKER_SCOPE_NAME
          value: "your-scope-name"
        - name: WORKER_GRPC_PORT
          value: "50052"
        - name: POD_NAME
          valueFrom:
            fieldRef:
              fieldPath: metadata.name
        - name: POD_IP
          valueFrom:
            fieldRef:
              fieldPath: status.podIP
        - name: WORKER_MIN_CONTEXTS
          value: "2"         # pre-created on startup; only useful with session_mode=reusable (default: 0)
        - name: WORKER_MAX_CONTEXTS
          value: "10"        # concurrent requests - but concurrent SESSIONS with session_mode=dedicated
        - name: WORKER_SESSION_MODE
          value: "reusable"  # or always_new, dedicated (default: reusable)
        - name: WORKER_HEADLESS
          value: "true"

        resources:
          requests:
            memory: "2Gi"
            cpu: "1000m"
          limits:
            memory: "4Gi"
            cpu: "2000m"

        securityContext:
          allowPrivilegeEscalation: false
          runAsUser: 1000

        volumeMounts:
        - name: dshm
          mountPath: /dev/shm

      volumes:
      - name: dshm
        emptyDir:
          medium: Memory
          sizeLimit: 2Gi
---
apiVersion: v1
kind: Service
metadata:
  name: browser-hive-worker
spec:
  type: ClusterIP
  selector:
    app: browser-hive-worker
    scope: your-scope-name
  ports:
  - port: 50052
    targetPort: 50052
    name: grpc
  - port: 9090
    targetPort: 9090
    name: metrics
```

### Coordinator Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: browser-hive-coordinator
spec:
  # Exactly one. Routing corrects the cached free-slot counts by the coordinator's own
  # in-flight requests, which is exact only while one replica sees all of them (CLAUDE.md,
  # "Free slots are the discovery cache corrected by..."). Sessions are also in-memory.
  replicas: 1
  selector:
    matchLabels:
      app: browser-hive-coordinator
  template:
    metadata:
      labels:
        app: browser-hive-coordinator
    spec:
      terminationGracePeriodSeconds: 60
      serviceAccountName: browser-hive-coordinator  # For K8s API access
      containers:
      - name: coordinator
        image: browser-hive-coordinator:latest
        ports:
        - containerPort: 50051
          name: grpc
          protocol: TCP
        # Prometheus metrics. The worker PodMonitor selects app: browser-hive-worker and
        # will not match this pod, so the coordinator needs its own scrape target on a
        # port named `metrics`. See METRICS.md ("Coordinator Metrics").
        - containerPort: 9090
          name: metrics
          protocol: TCP

        # TCP for the same reason as the worker: no grpc.health.v1 service
        readinessProbe:
          tcpSocket:
            port: 50051
          initialDelaySeconds: 3
          periodSeconds: 2
          timeoutSeconds: 3
          successThreshold: 1
          failureThreshold: 2

        livenessProbe:
          tcpSocket:
            port: 50051
          initialDelaySeconds: 10
          periodSeconds: 10
          timeoutSeconds: 5
          failureThreshold: 3

        lifecycle:
          preStop:
            exec:
              command: ["/bin/sh", "-c", "sleep 5"]

        env:
        - name: COORDINATOR_MODE
          value: "kubernetes"
        - name: COORDINATOR_GRPC_PORT
          value: "50051"
        # Metrics are on by default; both shown for clarity
        # - name: COORDINATOR_ENABLE_METRICS
        #   value: "true"
        # - name: COORDINATOR_METRICS_PORT
        #   value: "9090"
        - name: RUST_LOG
          value: "info"
        # Optional: set to "true" to surface (as WARN) the connect/stats/health-check
        # errors for terminating pods. Default (unset/"false") downgrades them to DEBUG
        # to avoid routine shutdown-churn noise. See "Terminating pod log noise" below.
        # - name: COORDINATOR_ENABLE_TERMINATING_POD_WARNINGS
        #   value: "false"

        resources:
          requests:
            memory: "512Mi"
            cpu: "500m"
          limits:
            memory: "1Gi"
            cpu: "1000m"
---
apiVersion: v1
kind: Service
metadata:
  name: browser-hive-coordinator
spec:
  type: ClusterIP
  selector:
    app: browser-hive-coordinator
  ports:
  - port: 50051
    targetPort: 50051
    name: grpc
  - port: 9090
    targetPort: 9090
    name: metrics
```

### Terminating pod log noise

During normal pod churn (scaling, rolling updates, spot reclaim), the coordinator
periodically logs warnings about workers it cannot reach:

- `Failed to get stats from worker ...` (worker discovery loop, every 10s)
- `Failed to connect to worker ...` / `Health check failed for worker ...` (health monitor, every 1s)

These are expected: a pod that has received SIGTERM keeps `phase=Running` (with
`metadata.deletionTimestamp` set) until its process exits, so its gRPC server may
already be gone while the coordinator still has it in the discovered set.

To keep logs clean, the coordinator detects terminating pods via the
`deletionTimestamp` already present in the K8s pod list (**no extra API calls**) and
**downgrades these warnings to DEBUG** for such pods. This is purely a log-level
change — it never affects routing or health decisions.

**`COORDINATOR_ENABLE_TERMINATING_POD_WARNINGS`** (optional, default `false`):
- `false` / unset: suppress the above warnings for terminating pods (log at DEBUG).
- `true`: emit them at WARN as before (useful for debugging shutdown/discovery issues).

Note: warnings for **non-terminating** pods are always logged at WARN, since those may
indicate a real problem.

### Service Account (for Coordinator K8s API Access)

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: browser-hive-coordinator
  namespace: default
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: browser-hive-coordinator
rules:
- apiGroups: [""]
  # Pods only: discovery lists pods labelled app=browser-hive-worker and connects to
  # pod IPs directly. It never reads Services or Endpoints.
  resources: ["pods"]
  verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: browser-hive-coordinator
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: browser-hive-coordinator
subjects:
- kind: ServiceAccount
  name: browser-hive-coordinator
  namespace: default
```

## Graceful Shutdown Behavior

### Worker Graceful Shutdown Timeline

```
t=0s:   Pod deleted → preStop (sleep 5) starts; the worker is still healthy and routed to

t=5s:   SIGTERM received
        → Worker sets is_ready = false, HealthCheck reports healthy=false
        → Worker cancels all operations: in-flight requests answer TERMINATING
          (aborted, not completed — the coordinator retries them elsewhere)

t=5-7s: Coordinator's health monitor (1s poll) stops routing to the pod;
        K8s removes it from Service endpoints (cosmetic - nothing routes via the Service)

t=5s+:  Worker waits for the cancelled handlers to return, then exits

t=60s:  SIGKILL sent (terminationGracePeriodSeconds, counted from preStop)
        → Pod force-terminated if still running
```

In-flight requests are **not** drained — see the open item in TODO.md.

### Coordinator Graceful Shutdown Timeline

```
t=0s:   SIGTERM received
        → Coordinator cancels token
        → Coordinator stops accepting new work

t=0-60s: Coordinator waits for active requests
         → Returns TERMINATING to new requests
         → Forwards worker responses (including TERMINATING)
         → Retries on healthy workers if worker returns TERMINATING

t=60s:  SIGKILL sent
        → Pod force-terminated
```

## Health Check Behavior

The Worker's `health_check()` gRPC method returns:
- `healthy: true` when worker is ready and not terminating
- `healthy: false` when worker receives SIGTERM

The Coordinator:
1. Discovers **pods** directly via the K8s API (`Api<Pod>`, label `app=browser-hive-worker`,
   every 10s) and connects to `pod_ip:50052`. It never reads Services or Endpoints, and it
   filters on `status.phase == Running` — **not** on the `Ready` condition
2. Runs background health cache (polls the worker's `HealthCheck` RPC every 1 second)
3. Filters workers by health cache when selecting
4. Falls back to all discovered workers if cache is empty
5. Retries on healthy workers if first returns TERMINATING

⚠️ **The readiness probe does not affect routing.** It removes the pod from the Service's
endpoints, but nothing in this system routes through that Service — the coordinator holds pod
IPs. What actually takes a worker out of routing is `HealthCheck` returning
`healthy = false`, which `run_worker`'s SIGTERM handler sets. Keep the probe for `kubectl`
visibility and for anything else that consumes the Service; do not rely on it to drain a pod.

## Spot Instance Configuration

For AWS spot instances or preemptible nodes, reduce `terminationGracePeriodSeconds`:

```yaml
terminationGracePeriodSeconds: 60  # Spot nodes often give 30-120s notice
```

This ensures graceful shutdown completes before spot termination.

## Monitoring

Workers expose Prometheus metrics on port 9090 at `/metrics`. See [METRICS.md](METRICS.md) for the full metric list and a KEDA-based autoscaling guide (scaling on busy/free browser context slots instead of CPU/RAM).

## Testing Graceful Shutdown

To test graceful shutdown in your cluster:

```bash
# Start a long request
grpcurl -d '{"scope_name":"test","url":"https://example.com","timeout_seconds":300}' \
  coordinator:50051 scraper.coordinator.ScraperCoordinator/ScrapePage

# In another terminal, delete the worker pod
kubectl delete pod browser-hive-worker-xxx

# The request should be retried on another worker and complete there
```

Expected behavior:
1. preStop sleeps 5 s, then the worker receives SIGTERM
2. The in-flight request is aborted with TERMINATING
3. Coordinator retries it on another healthy worker (if ≥ 10 s of its deadline remain)
4. Client receives the retried response, or a final TERMINATING error
5. The worker exits as soon as its cancelled handlers have returned (SIGKILL at 60 s otherwise)
