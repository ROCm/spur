# GitHub App setup for ARC (repository scope → powderluv/sglang)

ARC (the `gha-runner-scale-set` model) authenticates to GitHub with a GitHub App.
Create the App (UI flow — can't be fully API-automated). Consume the three resulting
values (App ID, Installation ID, private key) to create the k8s secret.

## 1. Create the App
1. Go to **https://github.com/settings/apps** → **New GitHub App** (creates it under your `powderluv` account).
2. **GitHub App name:** anything globally-unique, e.g. `arc-sglang-powderluv`.
3. **Homepage URL:** anything, e.g. `https://github.com/powderluv/sglang`.
4. **Webhook:** **uncheck "Active"** — ARC's scale-set listener long-polls, it does not need a webhook.
5. **Permissions → Repository permissions:**
   - **Administration: Read and write**  ← required for repo-scope runner registration
   - **Metadata: Read-only**  ← auto-selected/mandatory
   - *(optional, harmless)* Actions: Read-only
   - Leave everything else "No access".
6. **Where can this GitHub App be installed?** → **Only on this account**.
7. Click **Create GitHub App**.

## 2. Grab the App ID + private key
- On the App's page, note the **App ID** (a number near the top).
- Scroll to **Private keys** → **Generate a private key** → a `.pem` downloads. Keep it safe.

## 3. Install the App on the fork
1. On the App's page → **Install App** (left sidebar) → **Install** on your `powderluv` account.
2. Choose **Only select repositories** → select **`powderluv/sglang`** → **Install**.
3. After installing, the browser URL is `https://github.com/settings/installations/<INSTALLATION_ID>` —
   note that **Installation ID** (a number). (Or fetch it via the API once it's installed.)

## 4. Provide the three values securely
- **App ID** and **Installation ID**: record the numbers (they're not secret).
- **Private key**: **do NOT paste it in chat.** Save the `.pem` into the workspace at:
  ```
  ./sglang-app.private-key.pem
  ```
  Then create the k8s secret `sglang-arc-app` (keys `github_app_id`,
  `github_app_installation_id`, `github_app_private_key`) in the `arc-runners` namespace, and
  delete the key file. (It's gitignored so it can't be committed.)

## Next steps
```
kubectl -n arc-runners create secret generic sglang-arc-app \
  --from-literal=github_app_id=<APP_ID> \
  --from-literal=github_app_installation_id=<INSTALL_ID> \
  --from-file=github_app_private_key=./sglang-app.private-key.pem
helm install arc ...gha-runner-scale-set-controller       # ARC controller
helm install linux-mi35x-gpu-1 -f runner-scale-set-values.yaml ...gha-runner-scale-set   # GPU runners
```
