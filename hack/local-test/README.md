# local-test

Runs the full dabba quickstart end to end in a throwaway [Multipass](https://multipass.run/)
VM, using the **local** copies of `dabba-modules` and `dabba-gitops` — no GitHub, nothing
pushed. This is how you validate a change to the platform before publishing anything.

```bash
./run.sh          # launch VM, run quickstart, assert, delete VM
./run.sh --keep   # leave the VM up for inspection on failure
```

How it works:

1. Bundles the three sibling repos (`dabba`, `dabba-modules`, `dabba-gitops`) and copies
   them into a clean Ubuntu VM.
2. Inside the VM, builds the `dabba` CLI and runs `dabba up` for one environment named after
   the substrate, with `--modules-source` pointing at the local `dabba-modules` and
   `--gitops-seed` at the local `dabba-gitops` — so the in-cluster Forgejo is seeded from local
   files and the platform comes up with no external git at all.
3. Asserts podinfo serves the OpenBao-seeded banner, runs `dabba status`, then `dabba down` and
   checks the cluster is gone, then tears the VM down.

Needs only `multipass` on the host. Everything else (docker, kind, kubectl, opentofu, flux,
Forgejo) lives in the VM. Upstream charts/images are still pulled from the internet — only
*our* code stays local.
