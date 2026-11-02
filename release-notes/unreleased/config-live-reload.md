# Live reload of `spec.config`

## New Feature

`RestateCluster` gained `spec.configReload: restart | live`. The default, `restart`, is today's
behaviour: the config ConfigMap is named after its content hash, so every change to `spec.config`
rolls the StatefulSet. With `live`, the ConfigMap keeps a fixed name (`restate-config`) and is
updated in place; the kubelet refreshes the mounted file (typically within a minute) and the
Restate server's own config watcher reloads it, with no pod restart.

Only settings that Restate reads at runtime take effect this way (memory budgets, the log filter,
memtable sizes, and anything read through its live configuration). Settings read once at startup,
such as RocksDB open options or bind addresses, keep their old value until the next restart, and
the operator does not tell the two apart. Settings passed as environment variables are unaffected
and still need a restart. Switching `configReload` itself changes the ConfigMap name and so rolls
the pods once. If you manage CRDs yourself (`installCrds: false`), apply the new CRD before using
the field.
