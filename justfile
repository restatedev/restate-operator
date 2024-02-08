generate:
  cargo run --bin crdgen > yaml/crd.yaml

install-crd: generate
  kubectl apply -f yaml/crd.yaml
