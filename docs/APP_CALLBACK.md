# Schwab's callback URL rules

Schwab's own rules for an app's callback URL ("redirect_uri"), as the developer portal states them. To set up the callback for this SDK, including a trusted local certificate, see [the setup guide](SETUP_GUIDE.md#3-choose-how-the-sign-in-comes-back).

## Requirements

A callback URL is required when you create an app. Schwab redirects the user there after consent and grant. The only OAuth flow Schwab offers is the authorization code grant.

1. **Scheme.** Some lines of business require `https`; others allow `http` or other schemes. Individual Trader API apps use `https`.
2. Every callback URL is checked for basic URL structure.
3. Every callback URL is checked for special or unsupported characters.
4. If the authorization request sends no `redirect_uri`, Schwab uses the URL registered with the app. With several registered, that can fail, because Schwab cannot tell which one to use. This SDK always sends one.
5. The callback URL sent in the authorization request must be **identical** to one registered with the app.

## Several callback URLs for one app

An app can have several callback URLs, entered on the Create App or Modify App form:

1. Separate them with commas and **no spaces**, for example `https://127.0.0.1:8080,https://127.0.0.1`.
2. The field holds at most 256 characters. Contact Schwab support for more.

## Common errors

| Registered URL | URL sent in `authorize` | Result | Reason |
| --- | --- | --- | --- |
| `https://host/path` | `https://host/path` | Success | |
| `https://host/path` | `myapp://blah/bam` | Error: invalid URI specified | The scheme does not match |
| `myapp://blah/bam` | `https://host/path` | Error: invalid URI specified | The scheme does not match |
| `https://host/path` | `http://host/path` | Error: invalid URI specified | The scheme does not match (`https` and `http`) |
| `myapp://this/that` | `myapp://host/path` | Error: invalid URI specified | The path does not match |
| `myapp://this/that` | `myapp://this/that` | Success | |

A trailing `/` counts: `https://127.0.0.1:8080/` and `https://127.0.0.1:8080` are different URLs, and the mismatch shows up as an "Access Denied" page after sign-in. This SDK refuses a callback URL that ends in `/`.
