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

        # CRITICAL: gRPC readiness probe that calls health_check()
        # This ensures terminating workers are removed from Service endpoints
        readinessProbe:
          grpc:
            port: 50052
            service: scraper.worker.WorkerService  # Full protobuf service name
          initialDelaySeconds: 5
          periodSeconds: 2
          timeoutSeconds: 3
          successThreshold: 1
          failureThreshold: 2

        # Liveness probe to restart crashed workers
        livenessProbe:
          grpc:
            port: 50052
            service: scraper.worker.WorkerService
          initialDelaySeconds: 10
          periodSeconds: 10
          timeoutSeconds: 5
          failureThreshold: 3

        # Lifecycle hook for endpoint propagation delay
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
          value: "2"
        - name: WORKER_MAX_CONTEXTS
          value: "10"
        - name: WORKER_SESSION_MODE
          value: "reusable"  # or always_new, reusable_preinit (default: reusable)
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
          capabilities:
            add:
            - SYS_ADMIN  # Required for Chrome sandbox

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
  replicas: 2
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

        # gRPC readiness probe
        readinessProbe:
          grpc:
            port: 50051
            service: scraper.coordinator.ScraperCoordinator
          initialDelaySeconds: 3
          periodSeconds: 2
          timeoutSeconds: 3
          successThreshold: 1
          failureThreshold: 2

        livenessProbe:
          grpc:
            port: 50051
            service: scraper.coordinator.ScraperCoordinator
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
        - name: RUST_LOG
          value: "info"

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
```

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
  resources: ["pods", "services", "endpoints"]
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
t=0s:   SIGTERM received
        → Worker sets is_ready = false
        → Readiness probe starts failing immediately
        → Worker cancels all operations

t=2-5s: K8s removes pod from Service endpoints
        → New requests no longer routed to this worker

t=0-60s: Worker waits for active requests to complete
         → Returns TERMINATING to new requests
         → Completes in-flight requests normally

t=60s:  SIGKILL sent (terminationGracePeriodSeconds)
        → Pod force-terminated if still running
```

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
1. Uses K8s Service discovery (filtered by readiness probe)
2. Runs background health cache (polls every 1 second)
3. Filters workers by health cache when selecting
4. Falls back to all discovered workers if cache is empty
5. Retries on healthy workers if first returns TERMINATING

## Spot Instance Configuration

For AWS spot instances or preemptible nodes, reduce `terminationGracePeriodSeconds`:

```yaml
terminationGracePeriodSeconds: 60  # Spot nodes often give 30-120s notice
```

This ensures graceful shutdown completes before spot termination.

## Monitoring

Workers expose Prometheus metrics on port 9090:
- `browser_hive_worker_total_contexts{scope}` - Total contexts
- `browser_hive_worker_active_contexts{scope}` - Busy contexts
- `browser_hive_worker_available_slots{scope}` - Free slots
- `browser_hive_worker_requests_total{scope}` - Total requests
- `browser_hive_worker_requests_failed{scope}` - Failed requests

## Testing Graceful Shutdown

To test graceful shutdown in your cluster:

```bash
# Start a long request
grpcurl -d '{"scope_name":"test","url":"https://example.com","timeout_seconds":300}' \
  coordinator:50051 scraper.coordinator.ScraperCoordinator/ScrapePage

# In another terminal, delete the worker pod
kubectl delete pod browser-hive-worker-xxx

# The request should complete successfully or be retried on another worker
```

Expected behavior:
1. Worker receives SIGTERM
2. Worker returns TERMINATING for new requests
3. Coordinator retries on another healthy worker
4. Client receives successful response or final TERMINATING error
5. Worker waits for active requests to complete
6. Pod terminates after all requests finish (or 60s timeout)
