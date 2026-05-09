---
description: Deploy, build, and test on the remote host
---

# Deploy Skill

To build and test HEVCuum:

1. `./deploy.sh` - sync code and build
2. `./deploy.sh test` - sync code and run tests
3. `./deploy.sh run -- [args]` - sync code, build, and run with arguments

Example: `./deploy.sh run -- --config config.yaml /mnt/media/dumb-tv`

Always use deploy.sh. Never compile or run locally.
