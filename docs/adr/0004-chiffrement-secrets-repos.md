# ADR 0004 — Chiffrement des credentials/secrets au repos

- **Statut** : Accepté
- **Date** : 2026-05-21
- **Feature** : F6.3.a (cf. [ROADMAP](../../claude-code/ROADMAP.md)) — issue #63
- **Décideurs** : équipe Autome
- **Contexte ADR** : précise l'[ADR initial d'architecture](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial) (« credentials chiffrés »)

---

## 1. Contexte

La table `credentials` (`migrations/0004_credentials.sql`) stocke les
tokens des connecteurs tiers (Slack bot token, Gmail OAuth2 refresh,
GitHub PAT, Notion integration secret, …). Ces données sont **hautement
sensibles** : un dump SQL exposerait l'accès à toutes les intégrations
des utilisateurs.

> **Note (#251)** : cette table n'a jamais été câblée. Les tokens sont
> stockés dans `connector_tokens` (`migrations/0011_connector_tokens.sql`),
> chiffrés selon cette décision ; `credentials` est supprimée par
> `migrations/0023_drop_credentials.sql`.

On veut un chiffrement **au repos** qui :

- protège contre une fuite de la base (dump, snapshot, accès lecture
  seule par un attaquant) ;
- supporte la **rotation de clé** sans temps d'arrêt ;
- reste utilisable **localement en dev** (pas de dépendance forte à un
  KMS managé en environnement de développement) ;
- garde un format **versionné** pour pouvoir migrer d'algorithme ou de
  paramètres sans casser les enregistrements existants.

## 2. Options

| Stratégie | Avantages | Inconvénients |
|---|---|---|
| **AES-256-GCM + KeyProvider versionné** | AEAD standard, performant, large support (RustCrypto, ring, aws-lc) ; nonce 96 bits aléatoires sûr pour le débit attendu (~thousands/s) ; tag d'intégrité 128 bits ; format envelope auto-descriptif → rotation transparente | Nécessite de stocker une clé maîtresse quelque part (env, fichier, KMS) |
| AES-256-GCM-SIV | Déterministe, donc résiste aux nonces réutilisés | Latence supérieure ; déterminisme **leak les égalités** (deux secrets identiques → même cipher) ; non souhaité ici |
| ChaCha20-Poly1305 | AEAD aussi standard, perf sans AES-NI | Pas de gain sur serveur Linux x86_64 moderne (AES-NI) ; cible plus mobile |
| Envelope encryption complète (KEK/DEK) | Reduce la surface d'exposition de la KEK | Lourd à déployer en dev ; complexité d'implémentation supérieure |
| KMS managé (AWS KMS / GCP KMS) direct | Sécurité opérationnelle élevée | Dépendance externe ; latence ; pas adapté pour le dev local |

## 3. Décision

**Nous adoptons AES-256-GCM avec un [`KeyProvider`](../../crates/core/src/secrets/) versionné.**

Raisons :

1. **Standard éprouvé.** AES-256-GCM est l'AEAD recommandé par NIST
   (SP 800-38D) ; implémentation RustCrypto auditée, alignée avec le
   reste de notre stack (sha2/hmac/argon2 viennent du même écosystème).
2. **Rotation native.** Le format envelope embarque le `key_id` —
   chaque enregistrement sait avec quelle clé il a été chiffré. Ajouter
   une nouvelle clé = `add_key(id, bytes, current=true)`. Le re-chiffrement
   progressif est un simple `decrypt → encrypt` (via le helper
   `reencrypt`) qu'on peut lancer en batch hors charge.
3. **Compatible dev/CI/prod.** En dev, la clé maîtresse est tirée d'une
   variable d'env (`AUTOME_SECRETS_KEY_HEX`). En prod, le `KeyProvider`
   sera remplacé par une impl qui résout les clés via un KMS (AWS/GCP/
   Vault) — le code applicatif reste identique.
4. **Format auto-descriptif.** L'envelope `v1:<key_id>:<nonce>:<ct+tag>`
   permet une migration future vers `v2` (autre algo, autre format de
   nonce) sans casser les enregistrements existants. Le `decrypt` parse
   le préfixe pour router.

## 4. Conséquences

- **Format envelope** (texte, base64url-safe) — encodé en base64 standard
  pour stockage en colonne `TEXT` PostgreSQL :

  ```
  v1:<key_id>:<nonce_b64>:<ciphertext_and_tag_b64>
  ```

  - `v1` : version du format (préfixe fixe, autorisera `v2`/`v3`).
  - `<key_id>` : ASCII, identifie la clé dans le `KeyProvider`. Doit
    être stable dans le temps (le `key_id` ne doit jamais être réutilisé
    pour une autre clé).
  - `<nonce_b64>` : 12 octets aléatoires, base64-encodés.
  - `<ciphertext_and_tag_b64>` : ciphertext concaténé au tag GCM 128
    bits, base64-encodés.

- **Nonce** : généré par `OsRng` à chaque chiffrement. 96 bits aléatoires
  donnent une probabilité de collision négligeable sous le seuil
  recommandé NIST (~2³² messages avec la même clé). On rotate la clé
  bien avant d'atteindre ce volume.

- **Clé** : 32 octets (256 bits). Pour le dev/test, génération via
  `OsRng` au démarrage ou fournie via variable d'env hex-encodée. En
  prod, fournie par le KMS.

- **Échec d'authentification** = `Err(Error::Invalid)` côté `decrypt`.
  Le tag GCM est vérifié constant-time par `aes-gcm` ; toute altération
  (ciphertext, nonce, AAD) → rejet.

- **Pas d'AAD** dans cette première version (l'envelope est déjà
  auto-descriptive). Si on veut lier un cipher à un `record_id` pour
  éviter le swap inter-records, on ajoutera `aad: &[u8]` dans une
  itération future.

## 5. Références

- [NIST SP 800-38D](https://csrc.nist.gov/publications/detail/sp/800-38d/final) — AES-GCM
- [RFC 5116](https://datatracker.ietf.org/doc/html/rfc5116) — AEAD
- [`aes-gcm`](https://docs.rs/aes-gcm/) — implémentation RustCrypto
- [docs/ARCHITECTURE.md §7](../ARCHITECTURE.md#7-choix-techniques-structurants-adr-initial)
