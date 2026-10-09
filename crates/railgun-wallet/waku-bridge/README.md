# waku-bridge

Source of `../static/waku-bundle.js`, the js-waku light node the wallet page runs for the legacy
transport. The bundle is committed so that building the wallet needs no Node toolchain.

```sh
npm ci && npm run build
```

Pinned to `@waku/sdk` 0.0.36, the version of `@railgun-community/waku-broadcaster-client-web`
9.1.1. js-waku is dual-licensed MIT / Apache-2.0.
