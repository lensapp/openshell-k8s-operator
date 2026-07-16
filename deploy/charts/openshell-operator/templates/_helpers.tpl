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
Name of the issuer serve Service.
*/}}
{{- define "openshell-operator.issuerServiceName" -}}
{{- printf "%s-issuer" (include "openshell-operator.fullname" .) -}}
{{- end -}}

{{/*
Public issuer URL the gateway discovers. Defaults to the in-cluster Service DNS
of the bundled serve pod (http is accepted — no cert needed for the issuer).
*/}}
{{- define "openshell-operator.issuerUrl" -}}
{{- if .Values.auth.oidc.issuerUrl -}}
{{- .Values.auth.oidc.issuerUrl -}}
{{- else -}}
{{- printf "http://%s.%s.svc:8081" (include "openshell-operator.issuerServiceName" .) .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/*
Issuer serve selector labels (distinct component within the release).
*/}}
{{- define "openshell-operator.issuerSelectorLabels" -}}
app.kubernetes.io/name: {{ include "openshell-operator.name" . }}-issuer
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
