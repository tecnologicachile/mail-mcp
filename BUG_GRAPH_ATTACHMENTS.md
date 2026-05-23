# Bug: `graph_send_message` descarta silenciosamente adjuntos al usar `in_reply_to`

**Severidad:** alta — pérdida de datos sin error visible
**Reportado:** 2026-05-15 — Gustavo Quiero (Comercializadora Interandina)
**Cuenta afectada en producción:** `interandina` (gquiero@interandina.cl, OAuth2 / Microsoft Graph)

---

## Resumen

Cuando se llama `graph_send_message` con `in_reply_to` y `attachments` en el mismo request, el MCP responde `status: ok` pero el correo sale **sin adjunto** y como **single-part `text/html`** (sin `multipart/mixed`, sin `multipart/alternative`).

El receptor del correo no ve el archivo. El servidor no devuelve error alguno — `recipients_count` es correcto, `status` es `ok`. El usuario asume que el mensaje se envió correctamente.

---

## Reproducción

```jsonc
// Tool call
{
  "tool": "graph_send_message",
  "account_id": "interandina",
  "to": ["jmontesinos@interandina.cl"],
  "cc": ["mmaffet@interandina.cl"],
  "subject": "RE: Revisión …",
  "in_reply_to": "<CPUPR80MB83893E714727CDBB22DDF882CD042@CPUPR80MB8389.lamprd80.prod.outlook.com>",
  "references": "<CPUPR80MB83893E714727CDBB22DDF882CD042@CPUPR80MB8389.lamprd80.prod.outlook.com>",
  "body_text": "…",
  "body_html": "<p>…</p>",
  "attachments": [
    { "file_path": "/home/usuario/proyectos/zohodeskinterandina/informe_iso27001_zohodesk.pdf",
      "filename": "informe_iso27001_zohodesk.pdf",
      "content_type": "application/pdf" }
  ]
}
```

**Respuesta del MCP:**
```json
{ "status": "ok", "method": "microsoft_graph", "recipients_count": 4 }
```

**Estructura MIME real del mensaje enviado (descargada vía IMAP de `Elementos enviados`, UID 11353):**

```
Content-Type: text/html; charset="Windows-1252"
Content-Transfer-Encoding: quoted-printable
X-MS-Has-Attach:                  ← VACÍO
MIME-Version: 1.0
size_bytes: 2639                  ← imposible contener un PDF de 184 KB
```

No hay `multipart/mixed`, no hay `application/pdf`, no hay `text/plain`. El PDF se perdió.

---

## Causa raíz

### Bug primario — `src/graph.rs` `send_via_reply()` (líneas 336-420)

La ruta de respuesta sigue el patrón documentado: `createReply` → `PATCH` → `send`. El PATCH se construye así (líneas 376-386):

```rust
let patch_body = PatchDraftRequest {
    subject: params.subject.clone(),
    body: GraphBody { content_type, content },
    to_recipients: recipients(&params.to),
    cc_recipients: recipients(&params.cc),
    bcc_recipients: recipients(&params.bcc),
    attachments: build_attachments(&params.attachments),   // ← se incluye
};

let response = client
    .patch(&patch_url)
    .bearer_auth(access_token)
    .json(&patch_body)
    .send()
    .await…
```

**Microsoft Graph NO permite establecer `attachments` por `PATCH /me/messages/{id}`.** La propiedad `attachments` es de navegación, no escribible vía PATCH. Graph acepta el PATCH con código 2xx y **descarta silenciosamente** el campo `attachments`.

Referencia oficial: <https://learn.microsoft.com/en-us/graph/api/message-update> — la lista de propiedades escribibles en `Message` no incluye `attachments`. Para agregar adjuntos a un draft hay que usar el endpoint dedicado:

- `POST /me/messages/{id}/attachments` para archivos < 3 MB (inline en el JSON).
- Para archivos ≥ 3 MB: `POST /me/messages/{id}/attachments/createUploadSession` y subir por chunks.

### Bug secundario — `src/graph.rs` `resolve_body()` (líneas 150-156)

```rust
fn resolve_body(body_html: &Option<String>, body_text: &Option<String>) -> (&'static str, String) {
    match (body_html, body_text) {
        (Some(html), _) => ("HTML", sanitize_cdata(html)),
        (None, Some(text)) => ("Text", sanitize_cdata(text)),
        (None, None) => ("Text", String::new()),
    }
}
```

Cuando el llamador envía **ambos** `body_text` y `body_html`, el wrapper descarta `body_text` y solo se envía HTML. El destinatario que no renderiza HTML (clientes legacy, sandboxes, lectores accesibilidad) verá un cuerpo vacío.

Limitación real: `Message.body` en Graph es un único objeto `{contentType, content}`. No soporta multipart/alternative nativo. Pero el contrato actual del MCP (descripción del tool y `HARD RULE #1` que pide enviar ambos) sugiere multipart. **El tool acepta los dos campos sin advertir que solo uno saldrá.**

---

## Por qué nadie lo notó antes

1. `send_via_sendmail` (ruta sin `in_reply_to`) SÍ envía `attachments` en el JSON de `POST /me/sendMail` (línea 258) — y en ese endpoint Graph **sí** los acepta. Por eso el bug solo se manifiesta en el flujo de respuesta.
2. El test `send_mail_request_serializes_correctly` (línea 469) valida la **estructura de serde**, no que Graph respete los campos en el endpoint correspondiente.
3. No hay test de integración end-to-end con verificación post-envío (IMAP / `/me/messages/{id}` GET) para `send_via_reply`.

---

## Propuesta de corrección

### Bug primario

En `send_via_reply()`, separar el manejo de adjuntos:

1. Quitar `attachments` del `PatchDraftRequest`.
2. Después del PATCH exitoso, iterar los adjuntos y hacer:
   - Si `len(content_bytes_decoded) < 3 MB`: `POST /me/messages/{draft_id}/attachments` con cuerpo
     ```json
     { "@odata.type": "#microsoft.graph.fileAttachment",
       "name": "...", "contentType": "...", "contentBytes": "..." }
     ```
   - Si es mayor: `POST /me/messages/{draft_id}/attachments/createUploadSession` y subir por chunks de hasta 4 MB.
3. Recién entonces hacer `POST /me/messages/{draft_id}/send`.

Esqueleto sugerido (sustituye líneas 373-405):

```rust
// Step 2: PATCH the draft with body + recipients (sin attachments)
let patch_body = PatchDraftRequest {
    subject: params.subject.clone(),
    body: GraphBody { content_type, content },
    to_recipients: recipients(&params.to),
    cc_recipients: recipients(&params.cc),
    bcc_recipients: recipients(&params.bcc),
    // attachments: removido — Graph ignora PATCH sobre la nav property
};
// … PATCH …

// Step 2.5: Agregar cada adjunto vía endpoint dedicado
for att in &params.attachments {
    let attachment_url = format!("{}/me/messages/{}/attachments", GRAPH_API_BASE, draft_id);
    let body = serde_json::json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": att.filename,
        "contentType": att.content_type,
        "contentBytes": att.content_base64,
    });
    client.post(&attachment_url).bearer_auth(access_token).json(&body).send().await?;
    // TODO: createUploadSession si >3 MB
}

// Step 3: send …
```

Eliminar también el campo `attachments` de `PatchDraftRequest` (líneas 93-105) o dejarlo deprecado con `#[serde(skip)]`.

### Bug secundario

Dos opciones:

- **A (más simple):** documentar en el `description` del tool y en `HARD RULE #1` que, **al usar Graph**, si se envían `body_text` y `body_html` solo el HTML viaja, y que el cliente debe asumir esa limitación. Eliminar la sugerencia de enviar ambos cuando `send_with: graph_send_message`.
- **B (preferida):** cuando vienen ambos, generar el correo como **MIME multipart/alternative codificado en base64** y enviarlo vía `POST /me/sendMail` con `Content-Type: text/plain` y el MIME como cuerpo *raw* (Graph soporta esto si se pasa el header correcto y el body es el MIME serializado). Esta opción preserva el contrato actual.

---

## Workarounds mientras no esté corregido

1. **No usar `in_reply_to` cuando hay adjunto crítico.** Si la trazabilidad de hilo es necesaria, agregar `Re:` al subject manualmente — el hilo lógico se preserva por subject + recipients pero la cabecera `References` no.
2. **Verificar post-envío** que el mensaje en `Elementos enviados` contiene la cabecera `X-MS-Has-Attach: yes` y `Content-Type: multipart/mixed` antes de dar por exitoso el envío crítico.
3. **Para correos críticos**, usar la cuenta `soporteint` con `ews_send_message`… pero EWS tampoco expone parámetro `attachments` en este MCP (ver `src/server.rs` schema). Por ahora la única ruta confiable con adjuntos es SMTP, no disponible en cuentas Microsoft 365 con SMTP AUTH deshabilitado.

---

## Evidencia adjunta

- Tool call exitoso reportado: `2026-05-15T17:15:39Z`, `recipients_count: 4`, `status: ok`.
- Mensaje resultante en `Elementos enviados/11/11353`: tamaño 2 639 bytes, sin parte `application/pdf`, `X-MS-Has-Attach:` vacío.
- Quote del destinatario José Pablo Montesinos confirmando ausencia de adjunto (mensaje `imap:interandina:INBOX:14:73490`): *"Muchas gracias por la información, pero no está el archivo adjunto."*
- Archivo origen verificado en disco: `/home/usuario/proyectos/zohodeskinterandina/informe_iso27001_zohodesk.pdf` (184 869 bytes, MIME `application/pdf`, legible por el usuario que ejecuta el MCP).
