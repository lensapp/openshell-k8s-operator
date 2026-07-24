{{/*
Chart name, optionally overridden.
*/}}
{{- define "openshell-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name: release name prefixed with the chart name, unless
overridden. Truncated to 63 chars for Kubernetes name limits.
*/}}
{{- define "openshell-operator.fullname" -}}
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

{{/*
Common labels.
*/}}
{{- define "openshell-operator.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "openshell-operator.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels.
*/}}
{{- define "openshell-operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "openshell-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
ServiceAccount name to use.
*/}}
{{- define "openshell-operator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "openshell-operator.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Name of the Secret holding the operator's gateway bearer. In bundledOidc mode
the mint Job creates it; in byo mode the user supplies it. Empty means the
operator connects without credentials.
*/}}
{{- define "openshell-operator.tokenSecretName" -}}
{{- if eq .Values.auth.mode "bundledOidc" -}}
{{- printf "%s-token" (include "openshell-operator.fullname" .) -}}
{{- else -}}
{{- .Values.auth.byo.tokenSecret -}}
{{- end -}}
{{- end -}}

{{/*
Name of the ConfigMap the issuer publishes the JWKS + discovery doc into.
*/}}
{{- define "openshell-operator.jwksConfigMapName" -}}
{{- printf "%s-oidc-jwks" (include "openshell-operator.fullname" .) -}}
{{- end -}}

{{/*
Name of the issuer serve Service. Bundled mode uses a fixed name so the gateway
subchart can reference the issuer with a static literal (Helm can't template
subchart values); BYO mode uses the release-scoped name.
*/}}
{{- define "openshell-operator.issuerServiceName" -}}
{{- if .Values.gateway.bundled -}}
{{- "openshell-issuer" -}}
{{- else -}}
{{- printf "%s-issuer" (include "openshell-operator.fullname" .) -}}
{{- end -}}
{{- end -}}

{{/*
Public issuer URL the gateway discovers (http is accepted — the issuer serves
only public JWKS, no cert needed). Bundled mode uses the fixed same-namespace
short DNS that matches the gateway subchart's oidc.issuer literal; BYO mode uses
the release-scoped Service FQDN. An explicit auth.oidc.issuerUrl wins.
*/}}
{{- define "openshell-operator.issuerUrl" -}}
{{- if .Values.auth.oidc.issuerUrl -}}
{{- .Values.auth.oidc.issuerUrl -}}
{{- else if .Values.gateway.bundled -}}
{{- "http://openshell-issuer:8081" -}}
{{- else -}}
{{- printf "http://%s.%s.svc:8081" (include "openshell-operator.issuerServiceName" .) .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/*
Gateway endpoint the operator dials. Bundled mode derives it from the gateway
subchart's fixed Service (server-TLS on 8080); BYO mode requires gateway.endpoint.
*/}}
{{- define "openshell-operator.gatewayEndpoint" -}}
{{- if .Values.gateway.endpoint -}}
{{- .Values.gateway.endpoint -}}
{{- else if .Values.gateway.bundled -}}
{{- printf "https://openshell-gateway.%s.svc:8080" .Release.Namespace -}}
{{- else -}}
{{- fail "gateway.endpoint is required when gateway.bundled=false; set it, or leave gateway.bundled=true to install the bundled gateway" -}}
{{- end -}}
{{- end -}}

{{/*
Secret holding the gateway server-CA the operator trusts (key ca.crt). Bundled
mode defaults to the gateway's self-signed server-cert Secret; empty otherwise.
*/}}
{{- define "openshell-operator.gatewayCaSecret" -}}
{{- if .Values.gateway.caSecret -}}
{{- .Values.gateway.caSecret -}}
{{- else if .Values.gateway.bundled -}}
{{- "openshell-server-tls" -}}
{{- end -}}
{{- end -}}

{{/*
Admission-webhook resource names. The webhook config names are cluster-scoped,
so they carry the release-scoped fullname to stay unique across installs. The
operator injects the caBundle into the two configs by these exact names, so keep
them in sync with the constants in src/webhook.rs.
*/}}
{{- define "openshell-operator.webhookServiceName" -}}
{{- printf "%s-webhook" (include "openshell-operator.fullname" .) -}}
{{- end -}}
{{- define "openshell-operator.webhookSecretName" -}}
{{- printf "%s-webhook-tls" (include "openshell-operator.fullname" .) -}}
{{- end -}}
{{- define "openshell-operator.webhookMutatingName" -}}
{{- printf "%s-exec" (include "openshell-operator.fullname" .) -}}
{{- end -}}
{{- define "openshell-operator.webhookValidatingName" -}}
{{- printf "%s-guard" (include "openshell-operator.fullname" .) -}}
{{- end -}}

{{/*
Issuer serve selector labels (distinct component within the release).
*/}}
{{- define "openshell-operator.issuerSelectorLabels" -}}
app.kubernetes.io/name: {{ include "openshell-operator.name" . }}-issuer
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
