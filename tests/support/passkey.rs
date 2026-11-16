//! Test-only ES256 authenticator for backup flags and zero counters unsupported by SoftPasskey.
//! No production verifier bypass: responses go through the normal HTTP/WebAuthn signature checks.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto_glue::{
    ecdsa_p256::{
        self, EcdsaP256PrivateKey, EcdsaP256PublicSec1Point, EcdsaP256Signature,
        EcdsaP256SigningKey,
    },
    traits::Signer,
};
use rand_core::{OsRng, RngCore};
use serde_cbor_2::value::Value as Cbor;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const UP: u8 = 0x01;
pub const UV: u8 = 0x04;
pub const BE: u8 = 0x08;
pub const BS: u8 = 0x10;
const AT: u8 = 0x40;

pub struct TestPasskey {
    signing_key: EcdsaP256PrivateKey,
    id: [u8; 32],
}

impl TestPasskey {
    pub fn new() -> Self {
        let mut id = [0; 32];
        OsRng.fill_bytes(&mut id);
        Self {
            signing_key: ecdsa_p256::new_key(),
            id,
        }
    }

    pub fn id(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.id)
    }

    /// Standard privacy-preserving `fmt: none` registration, not an attested device claim.
    pub fn register(&self, options: &Value, origin: &str, flags: u8) -> Value {
        let point = EcdsaP256PublicSec1Point::from(self.signing_key.public_key());
        let cose = Cbor::Map(BTreeMap::from([
            (Cbor::Integer(1), Cbor::Integer(2)),  // EC2
            (Cbor::Integer(3), Cbor::Integer(-7)), // ES256
            (Cbor::Integer(-1), Cbor::Integer(1)), // P-256
            (Cbor::Integer(-2), Cbor::Bytes(point.x().unwrap().to_vec())),
            (Cbor::Integer(-3), Cbor::Bytes(point.y().unwrap().to_vec())),
        ]));
        let mut data = authenticator_data(
            options["publicKey"]["rp"]["id"].as_str().unwrap(),
            flags | AT,
            0,
        );
        data.extend_from_slice(&[0; 16]); // No identifying AAGUID or attestation certificate.
        data.extend_from_slice(&(self.id.len() as u16).to_be_bytes());
        data.extend_from_slice(&self.id);
        data.extend(serde_cbor_2::to_vec(&cose).unwrap());
        let attestation = Cbor::Map(BTreeMap::from([
            (Cbor::Text("fmt".into()), Cbor::Text("none".into())),
            (Cbor::Text("attStmt".into()), Cbor::Map(BTreeMap::new())),
            (Cbor::Text("authData".into()), Cbor::Bytes(data)),
        ]));
        json!({
            "id": self.id(), "rawId": self.id(), "type": "public-key", "extensions": {},
            "response": {
                "attestationObject": URL_SAFE_NO_PAD.encode(serde_cbor_2::to_vec(&attestation).unwrap()),
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client_data(options, origin, "webauthn.create")),
                "transports": ["internal"]
            }
        })
    }

    /// Flags and counter are signed, so negative UV/RP tests are not merely broken signatures.
    pub fn authenticate(&self, options: &Value, origin: &str, flags: u8, counter: u32) -> Value {
        let data = authenticator_data(
            options["publicKey"]["rpId"].as_str().unwrap(),
            flags,
            counter,
        );
        let client = client_data(options, origin, "webauthn.get");
        let mut signed = data.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let signature: EcdsaP256Signature = EcdsaP256SigningKey::from(&self.signing_key)
            .try_sign(&signed)
            .unwrap();
        json!({
            "id": self.id(), "rawId": self.id(), "type": "public-key", "extensions": {},
            "response": {
                "authenticatorData": URL_SAFE_NO_PAD.encode(data),
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client),
                "signature": URL_SAFE_NO_PAD.encode(signature.to_der().to_bytes()),
                "userHandle": null
            }
        })
    }
}

fn authenticator_data(rp: &str, flags: u8, counter: u32) -> Vec<u8> {
    let mut data = Sha256::digest(rp.as_bytes()).to_vec();
    data.push(flags);
    data.extend_from_slice(&counter.to_be_bytes());
    data
}

fn client_data(options: &Value, origin: &str, kind: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type": kind, "challenge": options["publicKey"]["challenge"], "origin": origin, "crossOrigin": false
    })).unwrap()
}
