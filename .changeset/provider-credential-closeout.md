---
"@mirri-ai/release": minor
---

A provider whose key gets rejected now shows a "needs reconfigure" badge in the settings page the moment the failure lands — with an action hint instead of a bare error — and the badge clears as soon as you save a new key.

The settings page manages provider credentials end to end: set a key or register the Azure CLI as an external source, remove a credential, and see type / source-presence / health per provider. Editing never echoes a stored key back into the form; an env reference you type is written to the provider's env table rather than the retired config key field. Changes made from the CLI surface in an open settings page without a restart.

`mirri credential list` gains a STATUS column (honestly "unknown" in direct mode) and an AZ-present flag for external entries; migration and its dry-run now name each skipped provider with handling guidance instead of a bare count.
