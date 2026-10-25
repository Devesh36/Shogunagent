#!/usr/bin/env bash
# Validate deploy/: the Helm chart (lint + render with the default values and every ci/*.yaml,
# schema-checked with kubeconform), the failure cases the chart must refuse, and the compose file.
# Run from the repository root:  deploy/check.sh
# Tools: HELM (default `helm`), KUBECONFORM (default `kubeconform`), docker with the compose plugin.
# A missing tool fails the check; it is never skipped silently.
set -euo pipefail

HELM=${HELM:-helm}
KUBECONFORM=${KUBECONFORM:-kubeconform}
K8S_VERSION=${K8S_VERSION:-1.29.0}
CHART=deploy/helm/shogunagent
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

echo "== helm lint / template (defaults)"
"$HELM" lint --strict "$CHART"
"$HELM" template ci "$CHART" > "$out/default.yaml"
"$KUBECONFORM" -strict -summary -kubernetes-version "$K8S_VERSION" "$out/default.yaml"

for values in "$CHART"/ci/*.yaml; do
  echo "== helm lint / template ($values)"
  "$HELM" lint --strict "$CHART" -f "$values"
  "$HELM" template ci "$CHART" -f "$values" > "$out/case.yaml"
  "$KUBECONFORM" -strict -summary -kubernetes-version "$K8S_VERSION" "$out/case.yaml"
done

echo "== settings the chart must refuse"
refuse() {
  if "$HELM" template ci "$CHART" "$@" > /dev/null 2> "$out/err"; then
    echo "expected a refusal for: $*" >&2
    exit 1
  fi
  echo "refused as expected: $* ($(grep -m1 "Error" "$out/err"))"
}
refuse --set replicaCount=2
refuse --set keyProvider.type=plaintext
refuse --set embedLocal.enabled=true
refuse --set secrets.create=true --set secrets.existingSecret=x

echo "== rendered invariants"
python3 - "$out/default.yaml" <<'PY'
import sys, yaml
docs = [d for d in yaml.safe_load_all(open(sys.argv[1])) if d]
sts = next(d for d in docs if d["kind"] == "StatefulSet")
pod = sts["spec"]["template"]["spec"]
assert pod["securityContext"]["runAsNonRoot"] is True
for c in pod["containers"]:
    sc = c["securityContext"]
    assert sc["readOnlyRootFilesystem"] is True, c["name"]
    assert sc["allowPrivilegeEscalation"] is False, c["name"]
    assert sc["capabilities"]["drop"] == ["ALL"], c["name"]
side = next(c for c in pod["containers"] if c["name"] == "sidecar")
assert "/v1/status" in " ".join(side["livenessProbe"]["exec"]["command"])
assert "/v1/status" in " ".join(side["readinessProbe"]["exec"]["command"])
assert {m["mountPath"] for m in side["volumeMounts"]} == {"/data", "/tmp"}
assert sts["spec"]["volumeClaimTemplates"][0]["metadata"]["name"] == "data"
assert not any(p.get("containerPort") == 7464 for c in pod["containers"] for p in c.get("ports", [])), "the sidecar port is never published"
np = next(d for d in docs if d["kind"] == "NetworkPolicy")
assert set(np["spec"]["policyTypes"]) == {"Ingress", "Egress"}
print("rendered invariants OK")
PY

echo "== docker compose config"
cp deploy/compose/.env.example "$out/.env"
docker compose --env-file "$out/.env" -f deploy/compose/docker-compose.yml config --quiet
echo "deploy check OK"
