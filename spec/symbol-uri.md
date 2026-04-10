# LIP Symbol URI Scheme

Reference for the `lip://` URI grammar (spec §5).

## Grammar

```
lip-uri      ::= "lip://" scope "/" package "@" version "/" path "#" descriptor
scope        ::= "npm" | "cargo" | "pub" | "pip" | "go" | "local" | "team" | "scip"
package      ::= UTF-8, no spaces, URL-encoded if necessary
version      ::= semver | content-hash
path         ::= relative path within package (forward slashes)
descriptor   ::= type-descriptor | method-descriptor | field-descriptor

type-descriptor   ::= identifier
method-descriptor ::= type "." method ["(" params ")"]
field-descriptor  ::= type "." field
```

## Examples

### External dependencies

```
lip://npm/react@18.2.0/index#useState
lip://npm/react@18.2.0/index#Component.setState
lip://cargo/tokio@1.35.1/runtime#Runtime
lip://cargo/tokio@1.35.1/runtime#Runtime.spawn(Future)
lip://pub/flutter@3.19.0/widgets#StatefulWidget
lip://pub/flutter@3.19.0/widgets#StatefulWidget.createState()
lip://pub/http@1.2.0/http#Client.get(Uri)
lip://pip/numpy@1.26.0/core#ndarray
lip://go/github.com.gin-gonic.gin@v1.9.0/gin#Engine.GET
```

### Local repository symbols

```
lip://local/myproject/lib/src/auth.dart#AuthService
lip://local/myproject/lib/src/auth.dart#AuthService.verifyToken(String)
```

### Team / private registry

```
lip://team/internal-api@2.1.0/models#UserRecord
```

## Descriptor escaping

Identifiers containing non-alphanumeric characters are backtick-escaped,
identical to SCIP's escaping rules:

```
lip://npm/lodash@4.17.21/lodash#`_.chunk`
```

## Validation rules

A LIP URI is **rejected** if it:

- Does not start with `lip://`
- Contains a null byte (`\0`)
- Contains a path traversal sequence (`..`)
- Is not valid UTF-8

These checks are implemented in:
- Rust: `crate::schema::types::LipUri::validate`
- Dart: `LipUri.parse` factory constructor

## Scope reference

| Scope   | Package manager          | Example                                 |
|---------|--------------------------|-----------------------------------------|
| `npm`   | Node.js / npm / yarn     | `lip://npm/lodash@4.17.21/lodash#chunk` |
| `cargo` | Rust / Cargo             | `lip://cargo/serde@1.0.0/lib#Serialize` |
| `pub`   | Dart / Flutter (pub.dev) | `lip://pub/http@1.2.0/http#Client`      |
| `pip`   | Python / pip             | `lip://pip/requests@2.31.0/api#get`     |
| `go`    | Go modules               | `lip://go/github.com.gin-gonic.gin@v1.9.0/gin#Engine` |
| `local` | Files in the local repo  | `lip://local/myapp/src/main.rs#main`    |
| `team`  | Private team registry    | `lip://team/shared-lib@1.0.0/lib#Util`  |
| `scip`  | Imported from SCIP index | `lip://scip/scip-typescript/npm/...`    |
