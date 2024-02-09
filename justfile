generate:
  cargo run --bin crdgen > crd/crd.yaml

generate-pkl:
  cargo run --bin schemagen | pkl eval crd/pklgen/generate.pkl -m crd

install-crd: generate
  kubectl apply -f crd/crd.yaml
