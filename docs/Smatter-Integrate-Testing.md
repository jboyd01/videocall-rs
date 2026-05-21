To test the smatter chat with access_token get from identity service, follow these steps:

Clone cc7 repo switch to branch <strong>feat/integrate-video-call</strong> open <b>cc7/src/features/auth/services/service.rs</b> 
```rust
  oidc_providers.insert(
            "cc7-keycloak".to_string(),
            OidcProvider {
                issuer_url: "http://host.docker.internal:8071".to_string(),
                client_id: "client_01KS5QSBVX44DCSFZRSR2WD9T6".to_string(),
                client_secret: Some("vJ6Xs-PV5hdN8Cx-jEK1KklVlpDI7SL0UQ5O09YCNyQ".to_string()),
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
client_id = "client_01KQV29RHWK1T13CEBDTBQT622"
client_secret="Yrvv-ETAXprnd2qE2RLiksmnGpA64q1_usqfG5-N_lM"
custom_audiences = ["<CC7 UI client_id>", "identity-service"]
```
Then run both the cc7 and jmap-chat-server, login to cc7 using the identity-service account, then in conversation page click on the video call icon to start a video call

Note: jmap-chat-server must be enabled SSE mode to receive the video call event
