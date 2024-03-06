{{- define "controller.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "controller.fullname" -}}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- $name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "controller.labels" -}}
{{- include "controller.selectorLabels" . }}
app.kubernetes.io/name: {{ include "controller.name" . }}
app.kubernetes.io/version: {{ .Values.version | default .Chart.Version | quote }}
{{- end }}

{{- define "controller.selectorLabels" -}}
app: {{ include "controller.name" . }}
{{- end }}

{{- define "controller.tag" -}}
{{- .Values.version | default .Chart.Version }}
{{- end }}
