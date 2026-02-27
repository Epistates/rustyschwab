```markdown
# App Callback URL Requirements and Limitations

## What are the requirements for the Callback URL ("redirect_uri")?

A Callback URL ("redirect_uri" parameter) is required when creating an App on the Developer Portal. This URL will be utilized in the OAuth `authorize` step to redirect the User from LMS, for "consent and grant," back to the calling App.

Per OAuth 2 requirements, a Callback URL is only applicable for the "authorization_code" and implicit flows ("grant_type"). Currently, the only available OAuth flow for Schwab APIs is the "authorization_code" Grant Type.

## Callback URL Requirements and Recommendations:

1. **URL Scheme**: Some LOBs require the Callback URL scheme to be secure (HTTPS). While other support HTTP or other URL schemes depending on specific business requirements.  
2. All callback URLs will be validated to ensure it meets basic URL structure.  
3. All callback URLs will be validated to ensure there are no special or unsupported characters in the address.  
4. If no Callback URL is sent during the OAuth flow, the value will automatically default to the Callback URL registered when the App was created on Schwab’s Dev Portal.  
   - In this scenario, if multiple Callback URLs were registered with the App, an error may be returned. The reason being the inability to determine which one to use since none was specified in the API request.  
5. The Callback URL sent during the OAuth flow must be identical to one of the Callback URL(s) registered with the App being used.

## Adding Multiple Callback URLs for a single App

Multiple URLs are supported for a single app. This can be done on either the Create App form or the Modify App forms.

**Note**:  
The table below will highlight some common permutations and the associated error reason information.

### To add multiple Callback URLs:
1. Enter Callback URLs by separating each with a comma. **NOTE**: do not separate the comma and the next URL with a space.  
   Example:  
   `https://www.example.com/path/page.etc,https://www.example.com/path2/page.etc`  
2. The field is currently limited at 256 characters max.  
   - Contact support if a special use-case occurs that exceeds this limitation.

## Common Callback URL Errors and Reasons:

| Registered URL             | URL Sent in OAuth `authorize` | Response or Error                                 | Reason                                                                 |
|---------------------------|-------------------------------|--------------------------------------------------|------------------------------------------------------------------------|
| `https://host/path`       | `https://host/path`          | Successful response                             |                                                                    |
| `https://host/path`       | `myapp://blah/bam`           | Error - invalid URI specified                  | Scheme sent does not match registered                                 |
| `myapp://blah/bam`        | `https://host/path`          | Error - invalid URI specified                  | Scheme sent does not match registered                                 |
| `https://host/path`       | `http://host/path`           | Error - invalid URI specified                  | Scheme sent does not match registered ("https" vs. "http")            |
| `myapp://this/that`       | `myapp://host/path`          | Error - invalid URI specified                  | Path sent does not match registered                                   |
| `myapp://this/that`       | `myapp://this/that`          | Successful response                           |                                                                    |

---

**Get Started**  
**3rd Party Company**  
**Individual**  
**Developer**  
**About the Individual Developer Role**  
**Become an Individual Developer**  
**APIs and Apps**  
**Create an App**  
**Modify an App**  
**Test in Sandbox**  
**Promoting Apps to Production**  
**OAuth Restart vs. Refresh**

---

**Terms of Use | Privacy Notice**  
© 2025 Charles Schwab & Co., Inc. All rights reserved. Member SIPC. Unauthorized access is prohibited. Usage is monitored.
```