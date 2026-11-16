# Services

Describe what a project runs while you work in `.ideae.toml` at its root, and
start it per task from Run ▾ in the branch bar:

```toml
[[service]]
name = "web"
command = "npm run dev -- --port {port}"
port = 3000
url = "http://localhost:{port}"
env = { PORT = "{port}" }
```

Each task has a port slot (0 for the main checkout, 1, 2… for worktrees) and
`{port}` is `port + 100 × slot`, so the same service runs in several tasks at
once, provided the program takes its port from the environment or its
arguments. A task's terminals, the agent's included, get `IDEAE_PORT_<NAME>`.

A service runs in its own terminal tab and survives a window restart; a port
that is taken is reported with the process holding it.
