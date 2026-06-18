# Contributing to dabba

Thank you for your interest. dabba is maintained as part of our own infrastructure and
shared in the hope it is useful. We welcome bug reports, questions, and documentation fixes.
For features, please open an issue first so we can align before you invest time — and note
that reviews may be slow. The maintainers make the final call on what fits the project.

## Reporting bugs

Open a [new issue](../../issues/new) with:

- A clear title and what you expected vs. what happened
- Steps to reproduce
- Your environment (OS, substrate — kind/k3d/minikube — OpenTofu and kubectl versions)
- Relevant output (`dabba up` logs, `dabba status`, `flux get kustomizations`, `kubectl get pods -A`)

## Suggesting features

Open an issue describing the problem you are solving and why. For anything beyond a small
fix, please wait for a maintainer to agree on the approach before opening a PR.

## Working in this repo

This is the umbrella repo: the tier-0 quickstart (`quickstart/`, OpenTofu) and the local-test
harness (`hack/local-test/`). The platform modules live in
[dabba-modules](https://github.com/spice-labs-inc/dabba-modules) and the gitops manifests in
[dabba-gitops](https://github.com/spice-labs-inc/dabba-gitops).

- `quickstart/` is plain OpenTofu you can read and run by hand; keep it that way
- `hack/local-test/run.sh [--substrate kind|k3d|minikube]` runs the whole thing in a clean
  Multipass VM with no GitHub — use it to validate changes end to end before opening a PR
- `hack/validate.sh` runs the cheap static checks (fmt + every kustomize overlay renders)

## Tests and CI

- `hack/validate.sh` — fmt + kustomize render; run it before every PR
- `.github/workflows/quickstart.yml` — brings the platform up on kind in CI and asserts the
  demo app serves the OpenBao-seeded banner
- Run shell scripts through `shellcheck`

## Opening a pull request

- Reference the issue you aligned on
- Explain why, link related issues, keep commits focused
- Ensure CI passes

## Licensing

Contributions are under the project's [Apache-2.0 license](LICENSE); by submitting, you agree
to license them under the same terms and confirm you have the right to.

## Community

Spice Labs open-source discussions are on Matrix at
https://matrix.to/#/#spice-labs:matrix.org
