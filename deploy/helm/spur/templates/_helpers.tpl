{{/*
Expand the name of the chart.
*/}}
{{- define "spur.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name. Truncated at 63 chars (DNS-1123 label limit).
*/}}
{{- define "spur.fullname" -}}
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
Chart label string (chart name + version, sanitized).
*/}}
{{- define "spur.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Standard labels applied to every object.
*/}}
{{- define "spur.labels" -}}
helm.sh/chart: {{ include "spur.chart" . }}
{{ include "spur.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: spur
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/*
Selector labels — stable across upgrades. Do NOT add helm.sh/chart or version
here; selectors are immutable for Deployments/StatefulSets.
*/}}
{{- define "spur.selectorLabels" -}}
app.kubernetes.io/name: {{ include "spur.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Per-component selector labels.
Usage: {{ include "spur.componentSelectorLabels" (dict "ctx" . "component" "controller") }}
*/}}
{{- define "spur.componentSelectorLabels" -}}
{{ include "spur.selectorLabels" .ctx }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/*
Per-component labels (selector labels + chart/version/part-of).
Usage: {{ include "spur.componentLabels" (dict "ctx" . "component" "controller") }}
*/}}
{{- define "spur.componentLabels" -}}
{{ include "spur.labels" .ctx }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{/*
Resolved image reference for a given component.
Falls back to the top-level .Values.image when the component override is empty.
Usage: {{ include "spur.image" (dict "ctx" . "component" .Values.controller) }}
*/}}
{{- define "spur.image" -}}
{{- $top := .ctx.Values.image -}}
{{- $c := .component.image | default dict -}}
{{- $repo := $c.repository | default $top.repository -}}
{{- $tag := $c.tag | default $top.tag | default .ctx.Chart.AppVersion -}}
{{- printf "%s:%s" $repo $tag -}}
{{- end -}}

{{/*
Resolved imagePullPolicy for a given component.
*/}}
{{- define "spur.imagePullPolicy" -}}
{{- $top := .ctx.Values.image -}}
{{- $c := .component.image | default dict -}}
{{- default $top.pullPolicy $c.pullPolicy -}}
{{- end -}}

{{/*
ServiceAccount name (created or referenced).
*/}}
{{- define "spur.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (printf "%s-spur" .Release.Name) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Controller headless service DNS — used by Raft peers and other daemons.
*/}}
{{- define "spur.controllerHost" -}}
{{- printf "spurctld.%s.svc.cluster.local" .Release.Namespace -}}
{{- end -}}

{{/*
Raft peers list (Helm-rendered TOML array string) based on replicaCount.
*/}}
{{- define "spur.raftPeers" -}}
{{- $ns := .Release.Namespace -}}
{{- $port := .Values.controller.service.raftPort | int -}}
{{- $peers := list -}}
{{- range $i, $_ := until (.Values.controller.replicaCount | int) -}}
{{- $peers = append $peers (printf "\"spurctld-%d.spurctld.%s.svc.cluster.local:%d\"," $i $ns $port) -}}
{{- end -}}
{{- join "\n" $peers -}}
{{- end -}}

{{/*
Accounting endpoint (used in spur.conf).
*/}}
{{- define "spur.accountingHost" -}}
{{- printf "spurdbd.%s.svc.cluster.local:%d" .Release.Namespace (.Values.accounting.service.port | int) -}}
{{- end -}}
