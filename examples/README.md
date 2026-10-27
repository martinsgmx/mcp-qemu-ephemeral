# Recipe examples

Recipes are the shared language of mcp-qemu-ephemeral: the same YAML/JSON
document describes how a VM is assembled whether it comes from a file you
wrote, the `recipe_apply` MCP tool, or the runner itself.

## Recipes as run records

Every one-shot run (`vm_test_script` MCP tool, `mcp-qemu-ephemeral test`
CLI command) writes its **effective recipe** into the run directory:

```
runs/<run_id>/
├── recipe.yaml    ← exactly how this VM was assembled (the "approach")
├── script.sh      ← the script that was injected
├── serial.log     ← full console transcript
├── report.md      ← human/agent-readable report
└── report.json    ← machine-readable report (status, errors, tail)
```

So when an agent later asks *what happened*, it can read the recipe to see
which ISO, which credentials, which shell — and `report.md`/`report.json`
for the outcome. The files in this directory are examples of both authoring
shapes:

- `alpine-live-iso.yaml` — ISO-only recipe (the default "test my script" path)
- `alpine-base-image.yaml` — disk-image recipe (an installed OS)
