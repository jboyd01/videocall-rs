To test the smatter chat with access_token get from identity service, follow these steps:

Clone cc7 repo switch to branch <strong>feat/integrate-video-call</strong> open <b>cc7/src/features/auth/services/service.rs</b> 
```
  oidc_providers.insert(
            "cc7-keycloak".to_string(),
            OidcProvider {
                issuer_url: "http://host.docker.internal:8071".to_string(),
                client_id: "<client-id>".to_string(),
                client_secret: Some("<your-client-secret>".to_string()),
                provider_metadata: None,
                // Stalwart validates Keycloak tokens directly via its OIDC directory
                // (userinfo endpoint). The access_token is the correct bearer — not
                // the id_token, and no CC7 middleware exchange is involved.
                jmap_uses_access_token: true,
            },
        );
```
Find and change the config above to your own

Then clone the <b>jmap.chat.next</b> switch to branch <strong>feat/integrate-videocall</strong>

Change these oidc config in setting.toml file inside server folder that inside jmap-chat-server folder

```toml
[[auth.oidc]]
nickname = "foundation"
issuer_url = "http://host.docker.internal:8071"
client_id = "<client-id>"
client_secret="<your-client-secret>"
custom_audiences = ["<CC7 UI client_id>", "identity-service"]
```
Then run both the cc7 and jmap-chat-server, login to cc7 using the identity-service account, then in conversation page click on the video call icon to start a video call

Note: jmap-chat-server must be enabled SSE mode to receive the video call event
