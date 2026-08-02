# Account Setup Guide

This guide walks you through configuring email accounts for mail-mcp.

## Quick Reference: Environment Variables

Each account uses a segment name (e.g., `DEFAULT`, `WORK`, `PERSONAL`):

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MAIL_IMAP_<SEG>_HOST` | Yes | — | IMAP server hostname |
| `MAIL_IMAP_<SEG>_PORT` | No | 993 | IMAP port |
| `MAIL_IMAP_<SEG>_USER` | Yes | — | Email address / username |
| `MAIL_IMAP_<SEG>_PASS` | Yes* | — | Password or App Password (*optional with OAuth2) |
| `MAIL_IMAP_<SEG>_SECURE` | No | true | Use TLS |
| `MAIL_SMTP_<SEG>_HOST` | No | — | SMTP server hostname |
| `MAIL_SMTP_<SEG>_PORT` | No | 587 | SMTP port |
| `MAIL_SMTP_<SEG>_USER` | No | — | SMTP username (usually same as IMAP) |
| `MAIL_SMTP_<SEG>_PASS` | No | — | SMTP password (*optional with OAuth2) |
| `MAIL_SMTP_<SEG>_SECURE` | No | starttls | `starttls`, `tls`, or `plain` |
| `MAIL_OAUTH2_<SEG>_PROVIDER` | No | — | `google` or `microsoft` |
| `MAIL_OAUTH2_<SEG>_CLIENT_ID` | No | — | OAuth2 client ID |
| `MAIL_OAUTH2_<SEG>_CLIENT_SECRET` | No | — | OAuth2 client secret (use `none` for public clients) |
| `MAIL_OAUTH2_<SEG>_REFRESH_TOKEN` | No | — | OAuth2 refresh token |

Global settings:

| Variable | Default | Description |
|----------|---------|-------------|
| `MAIL_IMAP_WRITE_ENABLED` | false | Enable IMAP write operations |
| `MAIL_SMTP_WRITE_ENABLED` | false | Enable SMTP send operations |
| `MAIL_SMTP_SAVE_SENT` | true | Save sent emails to IMAP Sent folder |
| `MAIL_SMTP_TIMEOUT_MS` | 30000 | SMTP operation timeout |

---

## Microsoft Personal (Hotmail / Outlook.com)

### IMAP (reading email)

Microsoft personal accounts require an **App Password** for IMAP access.

**Prerequisites:** Two-factor authentication (2FA) must be enabled.

**Step 1 — Enable 2FA** (skip if already enabled):
1. Go to https://account.microsoft.com/security
2. Click "Security" > enable "Two-step verification"

**Step 2 — Create an App Password:**
1. Go directly to: **https://account.live.com/proofs/AppPassword**
2. Sign in if prompted
3. Microsoft generates a 16-character password (e.g., `abcdefghijklmnop`)
4. Copy it — you won't see it again

**Step 3 — Configure:**
```env
MAIL_IMAP_DEFAULT_HOST=outlook.office365.com
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_USER=yourname@hotmail.com
MAIL_IMAP_DEFAULT_PASS=abcdefghijklmnop
MAIL_IMAP_DEFAULT_SECURE=true
```

### SMTP (sending email)

> **Important:** Microsoft has disabled SMTP AUTH for personal accounts (hotmail.com, outlook.com, live.com). App Passwords do NOT work for SMTP on personal accounts. This is a Microsoft policy, not a limitation of this server.

**Current workaround:** Use a different provider as SMTP relay (e.g., Zoho, Gmail) to send emails.

**Future:** Microsoft Graph API support for sending emails from personal accounts is planned.

---

## Microsoft 365 (Enterprise / Work / School)

Microsoft 365 business accounts can use SMTP AUTH if the admin has enabled it.

### Check with your IT admin:
- SMTP AUTH must be enabled for your mailbox in Exchange Online
- Admin portal: https://admin.microsoft.com > Users > Active users > Mail > Manage email apps

### With App Password (if 2FA enabled):
1. Go to: **https://mysignins.microsoft.com/security-info**
2. Add method > App password
3. Use that password for both IMAP and SMTP

### Configuration:
```env
MAIL_IMAP_WORK_HOST=outlook.office365.com
MAIL_IMAP_WORK_PORT=993
MAIL_IMAP_WORK_USER=you@company.com
MAIL_IMAP_WORK_PASS=your-app-password
MAIL_IMAP_WORK_SECURE=true

MAIL_SMTP_WORK_HOST=smtp.office365.com
MAIL_SMTP_WORK_PORT=587
MAIL_SMTP_WORK_USER=you@company.com
MAIL_SMTP_WORK_PASS=your-app-password
MAIL_SMTP_WORK_SECURE=starttls
```

### With OAuth2 (advanced):
If your organization has registered an Azure AD app with `SMTP.Send` and `IMAP.AccessAsUser.All` permissions:

```env
MAIL_OAUTH2_WORK_PROVIDER=microsoft
MAIL_OAUTH2_WORK_CLIENT_ID=your-app-client-id
MAIL_OAUTH2_WORK_CLIENT_SECRET=your-client-secret
MAIL_OAUTH2_WORK_REFRESH_TOKEN=your-refresh-token
```

When OAuth2 is configured, `MAIL_IMAP_WORK_PASS` and `MAIL_SMTP_WORK_PASS` become optional.

---

## Google Gmail

Gmail requires an **App Password** for IMAP/SMTP access (regular password is blocked).

**Prerequisites:** Two-factor authentication (2FA) must be enabled.

**Step 1 — Enable 2FA** (skip if already enabled):
1. Go to: **https://myaccount.google.com/signinoptions/two-step-verification**

**Step 2 — Create an App Password:**
1. Go directly to: **https://myaccount.google.com/apppasswords**
2. Name it (e.g., "mail-imap-mcp")
3. Google generates a 16-character password
4. Copy it

**Step 3 — Configure:**
```env
MAIL_IMAP_GMAIL_HOST=imap.gmail.com
MAIL_IMAP_GMAIL_PORT=993
MAIL_IMAP_GMAIL_USER=you@gmail.com
MAIL_IMAP_GMAIL_PASS=abcd efgh ijkl mnop
MAIL_IMAP_GMAIL_SECURE=true

MAIL_SMTP_GMAIL_HOST=smtp.gmail.com
MAIL_SMTP_GMAIL_PORT=587
MAIL_SMTP_GMAIL_USER=you@gmail.com
MAIL_SMTP_GMAIL_PASS=abcd efgh ijkl mnop
MAIL_SMTP_GMAIL_SECURE=starttls
```

---

## Apple iCloud

iCloud Mail requires an **App-Specific Password** for IMAP/SMTP access (regular Apple ID password is blocked for third-party clients).

**Prerequisites:** Two-factor authentication (2FA) must be enabled on the Apple ID.

**Step 1 — Create an App-Specific Password:**
1. Go to: **https://appleid.apple.com/account/manage**
2. Sign in, then open **Sign-In and Security** → **App-Specific Passwords**
3. Generate a password (name it e.g. "mail-mcp")
4. Copy the password — you will not see it again

**Step 2 — Configure:**

Use the full iCloud email as the username (`you@icloud.com`; also works for `@me.com` / `@mac.com`).

```env
MAIL_IMAP_ICLOUD_HOST=imap.mail.me.com
MAIL_IMAP_ICLOUD_PORT=993
MAIL_IMAP_ICLOUD_USER=you@icloud.com
MAIL_IMAP_ICLOUD_PASS=your-app-specific-password
MAIL_IMAP_ICLOUD_SECURE=true

MAIL_SMTP_ICLOUD_HOST=smtp.mail.me.com
MAIL_SMTP_ICLOUD_PORT=587
MAIL_SMTP_ICLOUD_USER=you@icloud.com
MAIL_SMTP_ICLOUD_PASS=your-app-specific-password
MAIL_SMTP_ICLOUD_SECURE=starttls
```

**Folder names:** iCloud uses `Sent Messages`, `Deleted Messages`, `Junk`, `Drafts`, and `Archive` (not Gmail-style `Sent` / `Trash`). Short aliases such as `Sent` and `Trash` resolve to the provider folder when listing/selecting mailboxes.

**Sent mail:** Third-party SMTP to iCloud does **not** auto-file a copy in Sent. Leave `MAIL_SMTP_SAVE_SENT` at its default (`true`) so the server APPENDs the sent message to `Sent Messages` after SMTP succeeds.

---

## Zoho Mail

Zoho supports standard password authentication for IMAP and SMTP.

```env
MAIL_IMAP_DEFAULT_HOST=imap.zoho.com
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_USER=you@yourdomain.com
MAIL_IMAP_DEFAULT_PASS=your-password
MAIL_IMAP_DEFAULT_SECURE=true

MAIL_SMTP_DEFAULT_HOST=smtp.zoho.com
MAIL_SMTP_DEFAULT_PORT=587
MAIL_SMTP_DEFAULT_USER=you@yourdomain.com
MAIL_SMTP_DEFAULT_PASS=your-password
MAIL_SMTP_DEFAULT_SECURE=starttls
```

> **Note:** If Zoho requires App-Specific Passwords, generate one at: https://accounts.zoho.com/home#security/security_mysessions

---

## Fastmail

```env
MAIL_IMAP_DEFAULT_HOST=imap.fastmail.com
MAIL_IMAP_DEFAULT_PORT=993
MAIL_IMAP_DEFAULT_USER=you@fastmail.com
MAIL_IMAP_DEFAULT_PASS=your-app-password
MAIL_IMAP_DEFAULT_SECURE=true

MAIL_SMTP_DEFAULT_HOST=smtp.fastmail.com
MAIL_SMTP_DEFAULT_PORT=587
MAIL_SMTP_DEFAULT_USER=you@fastmail.com
MAIL_SMTP_DEFAULT_PASS=your-app-password
MAIL_SMTP_DEFAULT_SECURE=starttls
```

Generate App Password at: **https://www.fastmail.com/settings/security/devicekeys**

---

## Microsoft Graph API (Sending from any Microsoft account)

Microsoft has disabled SMTP AUTH for personal accounts and many enterprise accounts.
The `graph_send_message` tool uses **Microsoft Graph API** (`POST /me/sendMail`) instead
of SMTP, which works for both personal and enterprise accounts.

### Quick setup (using public Client ID)

This uses a public Client ID from the email-oauth2-proxy project. No Azure registration needed.

**Step 1 — Get a refresh token via device code flow:**

Run this command (requires `python3` and `requests`):

```bash
python3 -c "
import requests, time
client_id = '9e5f94bc-e8a4-4e73-b8be-63364c29d753'
scope = 'https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/Mail.ReadWrite offline_access'
data = requests.post('https://login.microsoftonline.com/common/oauth2/v2.0/devicecode',
    data={'client_id': client_id, 'scope': scope}).json()
print(f\"Open: {data['verification_uri']}\nCode: {data['user_code']}\")
dc = data['device_code']
while True:
    time.sleep(5)
    td = requests.post('https://login.microsoftonline.com/common/oauth2/v2.0/token',
        data={'grant_type':'urn:ietf:params:oauth:grant-type:device_code','client_id':client_id,'device_code':dc}).json()
    if 'access_token' in td: print(f\"REFRESH_TOKEN={td['refresh_token']}\"); break
    elif td.get('error') != 'authorization_pending': print(f\"Error: {td}\"); break
"
```

**Step 2 — Open the URL shown, enter the code, and sign in with your Microsoft account.**

**Step 3 — Copy the REFRESH_TOKEN and configure:**

```env
MAIL_IMAP_OUTLOOK_HOST=outlook.office365.com
MAIL_IMAP_OUTLOOK_PORT=993
MAIL_IMAP_OUTLOOK_USER=you@hotmail.com
MAIL_IMAP_OUTLOOK_PASS=your-app-password
MAIL_IMAP_OUTLOOK_SECURE=true

MAIL_OAUTH2_OUTLOOK_PROVIDER=microsoft
MAIL_OAUTH2_OUTLOOK_CLIENT_ID=9e5f94bc-e8a4-4e73-b8be-63364c29d753
MAIL_OAUTH2_OUTLOOK_CLIENT_SECRET=none
MAIL_OAUTH2_OUTLOOK_REFRESH_TOKEN=<paste-your-refresh-token>
```

> **Note:** For IMAP on personal accounts, you still need an App Password
> (https://account.live.com/proofs/AppPassword). The OAuth2 config is used
> by `graph_send_message` for sending emails.

> **Note for enterprise accounts:** If your organization blocks the public Client ID,
> you need to create your own (see below).

### Creating your own Azure Client ID (advanced)

If the public Client ID stops working or your organization blocks it, register your own app:

**Step 1 — Go to Azure Entra (formerly Azure AD):**
1. Open: **https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade**
2. Sign in with an admin account (or your personal Microsoft account)

**Step 2 — Register a new application:**
1. Click **"New registration"**
2. Name: `mail-imap-mcp` (or any name you prefer)
3. Supported account types: choose based on your needs:
   - **"Personal Microsoft accounts only"** — for hotmail/outlook.com
   - **"Accounts in any organizational directory and personal Microsoft accounts"** — for both enterprise and personal
4. Redirect URI: select **"Public client/native"** and enter: `http://localhost:11080`
5. Click **"Register"**

**Step 3 — Note the Client ID:**
- After registration, copy the **"Application (client) ID"** from the overview page
- This is your `MAIL_OAUTH2_<SEG>_CLIENT_ID`

**Step 4 — Configure API permissions:**
1. Go to **"API permissions"** in the left menu
2. Click **"Add a permission"** → **"Microsoft Graph"** → **"Delegated permissions"**
3. Add these permissions:
   - `Mail.Send` — send emails via Graph API
   - `Mail.ReadWrite` — read/write mailbox (optional, for future features)
   - `IMAP.AccessAsUser.All` — IMAP access (only needed if using OAuth2 for IMAP too)
4. Click **"Add permissions"**
5. If you see "Admin consent required": ask your admin to click **"Grant admin consent"**

**Step 5 — Enable public client flow:**
1. Go to **"Authentication"** in the left menu
2. Scroll to **"Advanced settings"**
3. Set **"Allow public client flows"** to **Yes**
4. Click **"Save"**

**Step 6 — Get refresh token:**
- Use the device code flow script above, replacing the `client_id` with your new one
- Set `MAIL_OAUTH2_<SEG>_CLIENT_SECRET=none` (public client, no secret needed)

**Direct links:**
| Action | URL |
|--------|-----|
| Register new app | https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/CreateApplicationBlade/quickStartType~/null/isMSAApp~/false |
| View your apps | https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade |
| Personal account apps | https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/ApplicationsListBlade |

---

## Multi-Account Configuration

You can configure multiple accounts using different segment names:

```env
# Personal Gmail
MAIL_IMAP_GMAIL_HOST=imap.gmail.com
MAIL_IMAP_GMAIL_USER=personal@gmail.com
MAIL_IMAP_GMAIL_PASS=app-password-1

# Work Microsoft 365
MAIL_IMAP_WORK_HOST=outlook.office365.com
MAIL_IMAP_WORK_USER=me@company.com
MAIL_IMAP_WORK_PASS=app-password-2

# Zoho (as default)
MAIL_IMAP_DEFAULT_HOST=imap.zoho.com
MAIL_IMAP_DEFAULT_USER=info@mydomain.com
MAIL_IMAP_DEFAULT_PASS=zoho-password

# SMTP for sending (only Zoho and Work have SMTP)
MAIL_SMTP_DEFAULT_HOST=smtp.zoho.com
MAIL_SMTP_DEFAULT_USER=info@mydomain.com
MAIL_SMTP_DEFAULT_PASS=zoho-password
MAIL_SMTP_DEFAULT_SECURE=starttls

MAIL_SMTP_WORK_HOST=smtp.office365.com
MAIL_SMTP_WORK_USER=me@company.com
MAIL_SMTP_WORK_PASS=app-password-2
MAIL_SMTP_WORK_SECURE=starttls
```

Use `account_id` parameter in tool calls: `"account_id": "gmail"`, `"account_id": "work"`, or `"account_id": "default"`.

---

## Troubleshooting

### "basic authentication is disabled" (Microsoft)
Microsoft blocks SMTP AUTH for personal accounts and many enterprise accounts. Use `graph_send_message` tool instead — it uses Microsoft Graph API which works for all Microsoft account types.

### "Authentication unsuccessful" (Microsoft)
- Verify your App Password is correct (not your regular password)
- Ensure 2FA is enabled: https://account.microsoft.com/security
- Generate a new App Password: https://account.live.com/proofs/AppPassword

### "Application-specific password required" (Google)
- Enable 2FA: https://myaccount.google.com/signinoptions/two-step-verification
- Create App Password: https://myaccount.google.com/apppasswords

### "STARTTLS is not supported"
Check `MAIL_SMTP_<SEG>_SECURE` — use `tls` instead of `starttls` if the server requires direct TLS (port 465).

### SMTP send works but email not saved to Sent folder
Ensure `MAIL_SMTP_SAVE_SENT=true` and that the IMAP account has write access (`MAIL_IMAP_WRITE_ENABLED=true`).
