//! The ID token check of the ChatGPT login: RS256 over the account JWKS.
//!
//! An error names the check that failed, never the token, its claims or the
//! signature: the token is a bearer of the account identity.

use std::borrow::Cow;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

/// The RSA keys an ID token signature is checked with, as the JWKS lists them.
#[derive(Debug, Clone, Default)]
pub struct Jwks {
    /// The RSA keys, each with its decoded modulus and exponent.
    keys: Vec<Key>,
}

/// One RSA key of the JWKS, ready for `ring`.
#[derive(Debug, Clone)]
struct Key {
    /// The key id the token header names.
    kid: String,
    /// The modulus, big-endian.
    n: Vec<u8>,
    /// The public exponent, big-endian.
    e: Vec<u8>,
}

impl Jwks {
    /// The RSA keys of the JWKS JSON `text`. A key that is not RSA or that has
    /// no decodable modulus stays out; a set with no key at all is an error.
    pub fn parse(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Raw {
            keys: Vec<RawKey>,
        }
        #[derive(Deserialize)]
        struct RawKey {
            kid: Option<String>,
            kty: Option<String>,
            n: Option<String>,
            e: Option<String>,
        }
        let raw: Raw = serde_json::from_str(text)
            .map_err(|_| "the signing keys are not valid JSON".to_string())?;
        let keys: Vec<Key> = raw
            .keys
            .into_iter()
            .filter(|key| key.kty.as_deref() == Some("RSA"))
            .filter_map(|key| {
                let (Some(kid), Some(n), Some(e)) = (key.kid, key.n, key.e) else {
                    return None;
                };
                let (n, e) = (decode(&n)?, decode(&e)?);
                (!n.is_empty() && !e.is_empty()).then_some(Key { kid, n, e })
            })
            .collect();
        if keys.is_empty() {
            return Err("the signing keys hold no RSA key".to_string());
        }
        Ok(Self { keys })
    }

    /// The key with `kid`, if the set has one.
    fn key(&self, kid: &str) -> Option<&Key> {
        self.keys.iter().find(|key| key.kid == kid)
    }
}

/// The parts of an ID token a login keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdClaims {
    /// The account id the token was issued for.
    pub sub: String,
    /// The account address, when the account shared one.
    pub email: Option<String>,
    /// The time the token stops being valid, in seconds since the epoch.
    pub exp: u64,
}

/// Checks `token` against `jwks` and the login it belongs to.
///
/// `audience` is the client id the token was issued to, `nonce` the value this
/// login sent, and `now` the current time in seconds. The signature is checked
/// before any claim is read, so a claim of an unsigned token never counts.
pub fn verify_id_token(
    token: &str,
    jwks: &Jwks,
    issuer: &str,
    audience: &str,
    nonce: &str,
    now: u64,
) -> Result<IdClaims, String> {
    let mut parts = token.trim().split('.');
    let (Some(head), Some(body), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("id token: not a three part JWT".to_string());
    };

    let bytes = decode(head).ok_or_else(header_error)?;
    let header: Header<'_> = serde_json::from_slice(&bytes)
        .map_err(|_| "id token: the header is not JSON".to_string())?;
    if header.alg.as_deref() != Some("RS256") {
        return Err("id token: the alg is not RS256".to_string());
    }
    let kid = header
        .kid
        .ok_or_else(|| "id token: the header names no kid".to_string())?;
    let key = jwks
        .key(&kid)
        .ok_or_else(|| "id token: the kid is not in the JWKS".to_string())?;

    let signature =
        decode(signature).ok_or_else(|| "id token: the signature is not base64url".to_string())?;
    let signed = &token.trim()[..head.len() + 1 + body.len()];
    let public = ring::signature::RsaPublicKeyComponents {
        n: key.n.as_slice(),
        e: key.e.as_slice(),
    };
    public
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &signature,
        )
        .map_err(|_| "id token: the signature does not verify".to_string())?;

    let bytes = decode(body).ok_or_else(|| "id token: the payload is not base64url".to_string())?;
    let claims: Claims<'_> = serde_json::from_slice(&bytes)
        .map_err(|_| "id token: the payload is not JSON".to_string())?;
    if claims.iss.as_deref() != Some(issuer) {
        return Err("id token: the iss does not match".to_string());
    }
    let audience_matches = match &claims.aud {
        Some(Audience::One(one)) => one.as_ref() == audience,
        Some(Audience::Many(many)) => many.iter().any(|one| one.as_ref() == audience),
        None => false,
    };
    if !audience_matches {
        return Err("id token: the aud does not match".to_string());
    }
    let exp = claims
        .exp
        .ok_or_else(|| "id token: the payload has no exp".to_string())?;
    if exp <= now.saturating_sub(5) {
        return Err("id token: the token is expired".to_string());
    }
    if claims.nonce.as_deref() != Some(nonce) {
        return Err("id token: the nonce does not match".to_string());
    }
    let sub = claims
        .sub
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| "id token: the payload has no sub".to_string())?;
    Ok(IdClaims {
        sub: sub.into_owned(),
        email: claims.email.map(Cow::into_owned),
        exp,
    })
}

/// The JOSE header, of which only the two checked fields are read.
///
/// The fields are `Cow`, so a value JSON writes with an escape (a `\/` in a
/// kid, say) is a copy instead of a refused token.
#[derive(Deserialize)]
struct Header<'a> {
    #[serde(borrow)]
    alg: Option<Cow<'a, str>>,
    #[serde(borrow)]
    kid: Option<Cow<'a, str>>,
}

/// The claims the check reads, borrowed unless an escape forces a copy.
#[derive(Deserialize)]
struct Claims<'a> {
    #[serde(borrow)]
    iss: Option<Cow<'a, str>>,
    #[serde(borrow)]
    aud: Option<Audience<'a>>,
    exp: Option<u64>,
    #[serde(borrow)]
    nonce: Option<Cow<'a, str>>,
    #[serde(borrow)]
    sub: Option<Cow<'a, str>>,
    #[serde(borrow)]
    email: Option<Cow<'a, str>>,
}

/// The `aud` claim, a string or an array of strings.
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience<'a> {
    /// One audience.
    One(#[serde(borrow)] Cow<'a, str>),
    /// Several audiences.
    Many(#[serde(borrow)] Vec<Cow<'a, str>>),
}

/// The base64url, unpadded bytes of `text`.
fn decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.as_bytes()).ok()
}

/// The error of a header that is not base64url.
fn header_error() -> String {
    "id token: the header is not base64url".to_string()
}

/// The fixed test key pair, its JWKS and a signer.
///
/// Only tests and the `testing` feature compile this, so a release binary
/// never holds a private key. The key is a throwaway, made for these tests.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use ring::rand::SystemRandom;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    /// The PKCS#8 DER of the test key, base64, standard alphabet.
    const PRIVATE_KEY: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCjoWjuQwWYyH1g1/eub6/jITbWhG2u09zqzDWwwTSpipHZPd45NeAN9Yp1LDbOwZm/W+6i8LPklrNYdV/RZUySoWLzPyIJqphnyUFPQsGppe6bZJhOF5OWCnnlJIKI67XlyGynuft4YHjqjkK2ijQuvEUisAa7PMqgPjfoMgSezGp4RQtuKzttSp7ad1nrhyhVHbeXoAvZZczNcTXEn6f+9UWzRjif8j5dEJMkXtCLjHQd+99jsBZ+xDPcJabPHYX4RxZcb9JSSr0EDFSMUV2cHnwB0YzylpghtQjMIMgEhnWQXG/4j4981ZVFzrcWIrul88oyL6hkPKq6NCqfOXpbAgMBAAECggEAV9YRabT4kCcvhojoH+88T4RI16aG6k44TKIKQDO2fTwxRph7beKJv3SLOpLdaOdsDNjgpP7o0lBDPbzkDpd+H0gawdCmCOJWpX/XWyDOrK6yAyBg0aq+QtCk1mhe2fkNz6djDDY900SWI/LxxOMzV9MgmZo8Qs5DRvmfMTmIWWUtsoYRfIlLlcI8FTE87gjtoUiYrovX6xF31vUDKyVpJeJmWaK+TeEPqdVYNU7qjU0RkiJP1ZxWa9ooZVP8sI/8lqOvgLwUMU5Xbo0io7360w06SPabZAItCtCBqqh9KV1RJC03362e2aiI6lfFvYLtgnCIbeGZu8tpAa05yrzkwQKBgQDZNvQX+yt0KCAlgkGOLpmO39e6DzyXG5AnuNxQKynujw+2t9vKYtj6PR84ulOp2NYmTvrc3mjjRPhwnjzkFJtAoQ6FR7H0iIZfPCa6EYn9q+ixM+CO/QlRJaABo7DQmWP62BAy+GOLnZLcTYMKPSwpYdzd+zuZ1SfNwzTvRaWQgwKBgQDA2RWK58l9/AjmU7fNqhUDbBaeRTRmhBNF9EQjKRnUe6jPQEaocvO8Ql6bkQZZdbwawov7zB/YurVoBdhyIEN6TBIhGgNeQiMTm8uDthDwkNMbtQxK0Bu4s0Xsyu22QzXYwVVC37TNPiBT8jUHa6jreGjtN0I+BePIO3/rc0KXSQKBgQCn7uCQmdNinpMb7LSaJFT8LPswEg+aE+lDjJxoLrbghSJ09Xir4/g0n4S/6u475jTKsU0p7ZnR+8TH7M7O10+gi+QaDu/6SoL4phyaxmyU/o+MULtQUTkez6QBIHjXDYBKszjy7y6nBlBAmnyxMdIuvEZyKIZPzIhVgJIwWvxB2QKBgD2gYbj87y/abcGk+TS46HQqg2Pae69S/5G4SFnwFXgIeF6ajLnh2i/MCc2u35eQ/yaSCwfRKFArAjOWwp9d3hJj4ts/I9poSitG+xdBBFLMBRpyldYGgQVaDLtp8eLvwXJd/fAb6vw3oT8SRI31goqU9PrxULLfUPY4xV0pfRs5AoGAazG0HjLCoRs2IiaAldrJod0ft3j1cQsPMVDcN0qObCUIavZP0yahp6kBobgX3UXK84gBloZI9ovAasyiF03MyN38dBRPQsPAPp3h3EIdnBNombap2EqDFd0YtY3ulc4kx15KiT3FsyGgnpMmLE8LxBrokiTAbnSOEEUsqdDTGdM=";

    /// The key id of the test key, as [`JWKS`] lists it.
    pub const KID: &str = "reagent-test";

    /// The JWKS JSON of the test key, to serve to a login under test.
    pub const JWKS: &str = concat!(
        "{\"keys\":[{\"kty\":\"RSA\",\"kid\":\"reagent-test\",\"alg\":\"RS256\",\"use\":\"sig\",",
        "\"n\":\"o6Fo7kMFmMh9YNf3rm-v4yE21oRtrtPc6sw1sME0qYqR2T3eOTXgDfWKdSw2zsGZv1vuovCz5JazWHVf0WVMkqFi8z8iCaqYZ8lBT0LBqaXum2SYTheTlgp55SSCiOu15chsp7n7eGB46o5Ctoo0LrxFIrAGuzzKoD436DIEnsxqeEULbis7bUqe2ndZ64coVR23l6AL2WXMzXE1xJ-n_vVFs0Y4n_I-XRCTJF7Qi4x0HfvfY7AWfsQz3CWmzx2F-EcWXG_SUkq9BAxUjFFdnB58AdGM8paYIbUIzCDIBIZ1kFxv-I-PfNWVRc63FiK7pfPKMi-oZDyqujQqnzl6Ww\",",
        "\"e\":\"AQAB\"}]}"
    );

    /// A JWT with header `{"alg":"RS256","kid":...}` and `payload_json` as its
    /// claims, signed with the test key. `payload_json` must be one JSON
    /// object, for example `{"iss":"...","sub":"..."}`.
    pub fn sign(payload_json: &str) -> Result<String, String> {
        sign_with_header(
            payload_json,
            &format!("{{\"alg\":\"RS256\",\"kid\":\"{KID}\"}}"),
        )
    }

    /// A JWT with an arbitrary `header_json`, signed with the test key when
    /// the header names RS256 and unsigned otherwise. Tests use it to build
    /// the tokens a real login must refuse.
    pub fn sign_with_header(payload_json: &str, header_json: &str) -> Result<String, String> {
        let head = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
        let body = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let signed = format!("{head}.{body}");
        let signature = if header_json.contains("\"RS256\"") {
            let der = STANDARD
                .decode(PRIVATE_KEY)
                .map_err(|_| "the test key is not base64".to_string())?;
            let key = RsaKeyPair::from_pkcs8(&der)
                .map_err(|_| "the test key is not a PKCS#8 RSA key".to_string())?;
            let mut signature = vec![0u8; key.public().modulus_len()];
            key.sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "the test key cannot sign".to_string())?;
            URL_SAFE_NO_PAD.encode(&signature)
        } else {
            // A token with no RS256 header gets bytes that are no signature.
            URL_SAFE_NO_PAD.encode(b"not a signature")
        };
        Ok(format!("{signed}.{signature}"))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{JWKS, sign, sign_with_header};
    use super::*;

    /// Two in the afternoon of a fixed day, so a token never ages.
    const NOW: u64 = 1_800_000_000;

    fn jwks() -> Jwks {
        Jwks::parse(JWKS).expect("the test JWKS parses")
    }

    fn claims(iss: &str, aud: &str, nonce: &str, exp: u64) -> String {
        format!(
            "{{\"iss\":\"{iss}\",\"aud\":\"{aud}\",\"nonce\":\"{nonce}\",\"exp\":{exp},\"sub\":\"user-1\",\"email\":\"a@b.c\"}}"
        )
    }

    fn verify(token: &str, nonce: &str) -> Result<IdClaims, String> {
        verify_id_token(
            token,
            &jwks(),
            "https://auth.openai.com",
            "oaiapp_1",
            nonce,
            NOW,
        )
    }

    #[test]
    fn oauth_jwt_a_good_token_passes_with_its_claims() {
        let token = sign(&claims(
            "https://auth.openai.com",
            "oaiapp_1",
            "nonce-1",
            NOW + 3600,
        ))
        .expect("signs");
        let claims = verify(&token, "nonce-1").expect("the token passes");
        assert_eq!(claims.sub, "user-1");
        assert_eq!(claims.email.as_deref(), Some("a@b.c"));
        assert_eq!(claims.exp, NOW + 3600);
    }

    #[test]
    fn oauth_jwt_an_aud_array_with_the_client_id_passes() {
        let token = sign(&format!(
            "{{\"iss\":\"https://auth.openai.com\",\"aud\":[\"other\",\"oaiapp_1\"],\"nonce\":\"n\",\"exp\":{},\"sub\":\"s\"}}",
            NOW + 60
        ))
        .expect("signs");
        assert!(verify(&token, "n").is_ok());
    }

    #[test]
    fn oauth_jwt_a_wrong_nonce_fails_on_the_nonce() {
        let token = sign(&claims(
            "https://auth.openai.com",
            "oaiapp_1",
            "sent",
            NOW + 60,
        ))
        .expect("signs");
        let error = verify(&token, "other").unwrap_err();
        assert!(error.contains("nonce"), "{error}");
    }

    #[test]
    fn oauth_jwt_a_wrong_audience_fails_on_the_aud() {
        let token = sign(&claims(
            "https://auth.openai.com",
            "oaiapp_2",
            "n",
            NOW + 60,
        ))
        .expect("signs");
        let error = verify(&token, "n").unwrap_err();
        assert!(error.contains("aud"), "{error}");
    }

    #[test]
    fn oauth_jwt_a_wrong_issuer_fails_on_the_iss() {
        let token =
            sign(&claims("https://evil.example", "oaiapp_1", "n", NOW + 60)).expect("signs");
        let error = verify(&token, "n").unwrap_err();
        assert!(error.contains("iss"), "{error}");
    }

    #[test]
    fn oauth_jwt_an_expired_token_fails_on_the_exp() {
        for exp in [NOW - 10, NOW - 5, 0] {
            let token =
                sign(&claims("https://auth.openai.com", "oaiapp_1", "n", exp)).expect("signs");
            let error = verify(&token, "n").unwrap_err();
            assert!(error.contains("expired"), "{exp}: {error}");
        }
        // Five seconds of clock skew are allowed, not more.
        let token =
            sign(&claims("https://auth.openai.com", "oaiapp_1", "n", NOW - 4)).expect("signs");
        assert!(verify(&token, "n").is_ok());
    }

    #[test]
    fn oauth_jwt_alg_none_or_hs256_fails_on_the_alg() {
        let payload = claims("https://auth.openai.com", "oaiapp_1", "n", NOW + 60);
        for header in [
            "{\"alg\":\"none\",\"kid\":\"reagent-test\"}",
            "{\"alg\":\"HS256\",\"kid\":\"reagent-test\"}",
            "{\"kid\":\"reagent-test\"}",
        ] {
            let token = sign_with_header(&payload, header).expect("builds");
            let error = verify(&token, "n").unwrap_err();
            assert!(error.contains("alg"), "{header}: {error}");
            assert!(!error.contains("reagent-test"), "{error}");
            assert!(!error.contains(&payload), "{error}");
        }
    }

    #[test]
    fn oauth_jwt_an_unknown_kid_fails_on_the_kid() {
        let payload = claims("https://auth.openai.com", "oaiapp_1", "n", NOW + 60);
        let token =
            sign_with_header(&payload, "{\"alg\":\"RS256\",\"kid\":\"nope\"}").expect("builds");
        let error = verify(&token, "n").unwrap_err();
        assert!(error.contains("kid"), "{error}");
    }

    #[test]
    fn oauth_jwt_a_flipped_signature_byte_fails_on_the_signature() {
        let token = sign(&claims(
            "https://auth.openai.com",
            "oaiapp_1",
            "n",
            NOW + 60,
        ))
        .expect("signs");
        let (head, signature) = token.rsplit_once('.').expect("three parts");
        let mut bytes = URL_SAFE_NO_PAD
            .decode(signature.as_bytes())
            .expect("base64url");
        bytes[0] ^= 0x01;
        let flipped = format!("{head}.{}", URL_SAFE_NO_PAD.encode(&bytes));
        let error = verify(&flipped, "n").unwrap_err();
        assert!(error.contains("signature"), "{error}");
    }

    #[test]
    fn oauth_jwt_a_token_that_is_not_three_parts_fails() {
        let error = verify("only-one-part", "n").unwrap_err();
        assert!(error.contains("three part"), "{error}");
        let error = verify("", "n").unwrap_err();
        assert!(error.contains("three part"), "{error}");
    }

    #[test]
    fn oauth_jwt_a_missing_sub_fails_on_the_sub() {
        let token = sign(&format!(
            "{{\"iss\":\"https://auth.openai.com\",\"aud\":\"oaiapp_1\",\"nonce\":\"n\",\"exp\":{}}}",
            NOW + 60
        ))
        .expect("signs");
        let error = verify(&token, "n").unwrap_err();
        assert!(error.contains("sub"), "{error}");
    }

    #[test]
    fn oauth_jwt_escaped_claims_are_read() {
        // The same issuer, nonce and address as the good token, written with
        // the `\/` escape JSON allows: a borrowed `&str` field refuses them.
        let token = sign(&format!(
            "{{\"iss\":\"https:\\/\\/auth.openai.com\",\"aud\":\"oaiapp_1\",\"nonce\":\"n\\/1\",\"exp\":{},\"sub\":\"user\\/1\",\"email\":\"a\\/b@c.d\"}}",
            NOW + 60
        ))
        .expect("signs");
        let claims = verify(&token, "n/1").expect("the escaped claims are read");
        assert_eq!(claims.sub, "user/1");
        assert_eq!(claims.email.as_deref(), Some("a/b@c.d"));
        assert_eq!(claims.exp, NOW + 60);
        // An escaped audience is read too, in its string and its array form.
        for aud in ["\"oaiapp\\/1\"", "[\"other\",\"oaiapp\\/1\"]"] {
            let token = sign(&format!(
                "{{\"iss\":\"https:\\/\\/auth.openai.com\",\"aud\":{aud},\"nonce\":\"n\",\"exp\":{},\"sub\":\"s\"}}",
                NOW + 60
            ))
            .expect("signs");
            assert!(
                verify_id_token(
                    &token,
                    &jwks(),
                    "https://auth.openai.com",
                    "oaiapp/1",
                    "n",
                    NOW
                )
                .is_ok(),
                "{aud}"
            );
        }
    }

    #[test]
    fn oauth_jwt_keys_that_are_not_rsa_are_left_out() {
        assert!(Jwks::parse("{\"keys\":[]}").is_err());
        assert!(Jwks::parse("not json").is_err());
        let mixed = concat!(
            "{\"keys\":[",
            "{\"kty\":\"EC\",\"kid\":\"a\",\"n\":\"AQAB\",\"e\":\"AQAB\"},",
            "{\"kty\":\"RSA\",\"kid\":\"b\",\"n\":\"!!\",\"e\":\"AQAB\"},",
            "{\"kty\":\"RSA\",\"kid\":\"reagent-test\",\"n\":\"AQAB\",\"e\":\"AQAB\"}",
            "]}"
        );
        let parsed = Jwks::parse(mixed).expect("one RSA key is left");
        assert_eq!(parsed.keys.len(), 1);
        assert_eq!(parsed.keys[0].kid, "reagent-test");
    }
}
