<!-- rules: parent=../CLAUDE.md owner=amux-vision enforces=A3,A7,A11 -->
# cloud (cloud.amux.io)

Read `cloud/README.md` first. `cloud.amux.io` is a live product with customers on it.

- The server is the same code as on a Mac (A7): no `if IS_CLOUD`. A difference comes from an
  environment variable or a gateway header.
- The gateway (`gateway/gateway.py`) is the trust boundary, and deploys go through the
  workflows in `.github/workflows/` (`deploy-cloud.yml`, `cloud-image-main.yml`). A change that
  can affect customers' workspaces or data is the owner's call (A3).
