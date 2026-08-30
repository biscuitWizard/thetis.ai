---
name = "Deploying and releasing mooR"
brief = "What deploy/ and the Dockerfiles contain, how a release is gated, how keys and tokens are handled, and what an operator needs to bring a real world up."
when_to_use = "Use when you touch a compose file, a Kubernetes manifest, a Dockerfile, a deployment script or a Debian package, because CI renders all of these on every push. Use it also when planning a real deployment: which shape to pick, how keys are made, and what must never reach a diff. Not for the development loop, and not for the Torchship database."
universal = false
tags = ["moor", "deploy", "deployment", "release", "docker", "docker-compose", "kubernetes", "debian package", "systemd", "nginx", "tls", "enrollment token", "signing key", "backup", "ghcr"]
related = ["moor/services/daemon-and-rpc", "moor/content-pipeline/cores-and-bootstrap"]
version = 2
---
