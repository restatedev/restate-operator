generate:
  cargo run --bin crdgen > yaml/crd.yaml
  cargo run --bin schemagen | pkl eval yaml/pklgen/generate.pkl --project-dir yaml/pklgen -m yaml

install-crd: generate
  kubectl apply -f yaml/crd.yaml
