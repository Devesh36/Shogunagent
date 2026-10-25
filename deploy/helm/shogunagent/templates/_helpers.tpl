{{- define "shogunagent.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "shogunagent.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "shogunagent.selectorLabels" -}}
app.kubernetes.io/name: {{ include "shogunagent.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "shogunagent.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "shogunagent.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
shogunagent.dev/region: {{ .Values.region | quote }}
{{- end -}}

{{- define "shogunagent.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "shogunagent.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "shogunagent.secretName" -}}
{{- if .Values.secrets.existingSecret -}}
{{- .Values.secrets.existingSecret -}}
{{- else if .Values.secrets.create -}}
{{- printf "%s-secrets" (include "shogunagent.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "shogunagent.image" -}}
{{- $tag := default (printf "v%s" .Chart.AppVersion) .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}

{{/* Fail early on settings that cannot work, naming the value. */}}
{{- define "shogunagent.validate" -}}
{{- $types := list "env" "aws-kms" "gcp-kms" "vault" -}}
{{- if not (has .Values.keyProvider.type $types) -}}
{{- fail (printf "keyProvider.type must be one of %s" (join ", " $types)) -}}
{{- end -}}
{{- if and (gt (int .Values.replicaCount) 1) (not .Values.sharding.acknowledged) -}}
{{- fail "replicaCount > 1 needs the principal sharding model in deploy/README.md; set sharding.acknowledged=true once your router pins each principal to one replica" -}}
{{- end -}}
{{- if and .Values.secrets.create .Values.secrets.existingSecret -}}
{{- fail "set either secrets.existingSecret or secrets.create, not both" -}}
{{- end -}}
{{- end -}}

{{/* GET /v1/status over loopback; succeeds only on HTTP 200. */}}
{{- define "shogunagent.statusProbe" -}}
exec:
  command:
    - /bin/bash
    - -c
    - >-
      exec 3<>/dev/tcp/127.0.0.1/{{ .Values.sidecar.port }} &&
      printf 'GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3 &&
      IFS= read -r line <&3 && [[ "$line" == *" 200 "* ]]
{{- end -}}
