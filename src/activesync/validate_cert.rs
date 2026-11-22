use crate::{
    config::Config,
    wbxml::{
        self,
        eas::{self, validate_cert as vc, DocumentBuilder},
        token::Token,
        Document, Node,
    },
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rustls_pki_types::{CertificateDer, UnixTime};
use x509_parser::{parse_x509_certificate, parse_x509_crl, pem::Pem};

fn is(doc: &Document, token: Token) -> bool {
    matches!(doc.nodes.first(), Some(Node::Start(start)) if start.code_page == token.code_page && start.token == token.token)
}

fn certificates(doc: &Document) -> Result<Vec<Vec<u8>>, ()> {
    let mut output = Vec::new();
    for child in eas::child_elements(doc) {
        if !is(&child, vc::CERTIFICATE) {
            return Err(());
        }
        let text = eas::find_text_after(&child, vc::CERTIFICATE).ok_or(())?;
        let normalized: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let der = STANDARD.decode(normalized).map_err(|_| ())?;
        if der.len() < 4 || output.len() >= 100 {
            return Err(());
        }
        output.push(der);
    }
    if output.is_empty() {
        return Err(());
    }
    Ok(output)
}

struct Request {
    targets: Vec<Vec<u8>>,
    chain: Vec<Vec<u8>>,
    check_crl: bool,
}

fn request(doc: &Document) -> Result<Request, ()> {
    if !is(doc, vc::VALIDATE_CERT) {
        return Err(());
    }
    let mut targets = None;
    let mut chain = None;
    let mut check_crl = false;
    let mut check_seen = false;
    for child in eas::child_elements(doc) {
        if is(&child, vc::CERTIFICATES) && targets.is_none() {
            targets = Some(certificates(&child)?);
        } else if is(&child, vc::CERTIFICATE_CHAIN) && chain.is_none() {
            chain = Some(certificates(&child)?);
        } else if is(&child, vc::CHECK_CRL) && !check_seen {
            check_seen = true;
            if !matches!(eas::find_text_after(&child, vc::CHECK_CRL), Some("0" | "1")) {
                return Err(());
            }
            check_crl = eas::find_text_after(&child, vc::CHECK_CRL) == Some("1");
        } else {
            return Err(());
        }
    }
    Ok(Request {
        targets: targets.ok_or(())?,
        chain: chain.unwrap_or_default(),
        check_crl,
    })
}

fn pem_file(path: &str, label: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let bytes = std::fs::read(path)?;
    let mut output = Vec::new();
    for item in Pem::iter_from_buffer(&bytes) {
        let item = item.map_err(|error| anyhow::anyhow!("invalid PEM: {error}"))?;
        if item.label == label {
            output.push(item.contents);
        }
    }
    anyhow::ensure!(!output.is_empty(), "PEM file has no {label} entries");
    Ok(output)
}

fn status(
    leaf: &[u8],
    chain: &[Vec<u8>],
    roots: &[Vec<u8>],
    crls: &[Vec<u8>],
    check_crl: bool,
) -> &'static str {
    let Ok((remaining, parsed)) = parse_x509_certificate(leaf) else {
        return "5";
    };
    if !remaining.is_empty() {
        return "5";
    }
    match parsed.key_usage() {
        Ok(Some(usage)) if !usage.value.digital_signature() && !usage.value.non_repudiation() => {
            return "6"
        }
        Err(_) => return "5",
        _ => {}
    }
    let leaf_der = CertificateDer::from(leaf);
    let Ok(cert) = webpki::EndEntityCert::try_from(&leaf_der) else {
        return "5";
    };
    let roots: Vec<_> = roots
        .iter()
        .map(|root| CertificateDer::from(root.as_slice()))
        .collect();
    let anchors: Vec<_> = roots
        .iter()
        .filter_map(|root| webpki::anchor_from_trusted_cert(root).ok())
        .collect();
    if anchors.is_empty() {
        return "17";
    }
    let chain: Vec<_> = chain
        .iter()
        .map(|item| CertificateDer::from(item.as_slice()))
        .collect();
    let now_seconds = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(now) => now.as_secs(),
        Err(_) => return "17",
    };
    for der in crls {
        let Ok((remaining, crl)) = parse_x509_crl(der) else {
            return "17";
        };
        if !remaining.is_empty() {
            return "16";
        }
        if check_crl && crl.last_update().timestamp() > now_seconds as i64 {
            return "16";
        }
    }
    let parsed_crls: Vec<webpki::CertRevocationList<'_>> = match crls
        .iter()
        .map(|der| webpki::BorrowedCertRevocationList::from_der(der).map(Into::into))
        .collect::<Result<_, _>>()
    {
        Ok(crls) => crls,
        Err(_) => return "17",
    };
    let refs: Vec<_> = parsed_crls.iter().collect();
    let revocation = if refs.is_empty() {
        None
    } else {
        Some(
            webpki::RevocationOptionsBuilder::new(&refs)
                .expect("nonempty CRLs")
                .with_expiration_policy(webpki::ExpirationPolicy::Enforce)
                .build(),
        )
    };
    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(now) => UnixTime::since_unix_epoch(now),
        Err(_) => return "17",
    };
    const EMAIL_PROTECTION: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x04];
    match cert.verify_for_usage(
        webpki::ALL_VERIFICATION_ALGS,
        &anchors,
        &chain,
        now,
        webpki::KeyUsage::required_if_present(EMAIL_PROTECTION),
        revocation,
        None,
    ) {
        Ok(_) if refs.is_empty() => {
            if check_crl {
                "16"
            } else {
                "1"
            }
        }
        Ok(_) => "1",
        Err(webpki::Error::UnknownIssuer) => "4",
        Err(webpki::Error::CertExpired { .. } | webpki::Error::CertNotValidYet { .. }) => "7",
        Err(webpki::Error::InvalidSignatureForPublicKey) => "3",
        Err(webpki::Error::RequiredEkuNotFoundContext(_)) => "6",
        Err(webpki::Error::CertRevoked) => {
            let leaf_only = webpki::RevocationOptionsBuilder::new(&refs)
                .expect("revocation requires a CRL")
                .with_depth(webpki::RevocationCheckDepth::EndEntity)
                .with_expiration_policy(webpki::ExpirationPolicy::Enforce)
                .build();
            match cert.verify_for_usage(
                webpki::ALL_VERIFICATION_ALGS,
                &anchors,
                &chain,
                now,
                webpki::KeyUsage::required_if_present(EMAIL_PROTECTION),
                Some(leaf_only),
                None,
            ) {
                Err(webpki::Error::CertRevoked) => "13",
                _ => "15",
            }
        }
        Err(webpki::Error::UnknownRevocationStatus | webpki::Error::CrlExpired { .. }) => {
            if check_crl {
                "16"
            } else {
                "1"
            }
        }
        Err(webpki::Error::CaUsedAsEndEntity | webpki::Error::EndEntityUsedAsCa) => "11",
        Err(_) => "5",
    }
}

pub(super) fn handle(config: &Config, document: &Document) -> Response {
    let mut b = DocumentBuilder::new();
    b.start(vc::VALIDATE_CERT);
    match request(document) {
        Err(()) => {
            b.leaf(vc::STATUS, "17");
        }
        Ok(Request {
            targets,
            chain,
            check_crl,
        }) => {
            let roots = pem_file(&config.validate_cert_ca_file, "CERTIFICATE");
            let crls = config
                .validate_cert_crl_file
                .as_deref()
                .map(|path| pem_file(path, "X509 CRL"))
                .transpose();
            match (roots, crls) {
                (Ok(roots), Ok(crls)) => {
                    b.leaf(vc::STATUS, "1");
                    let crls = crls.unwrap_or_default();
                    for leaf in targets {
                        b.start(vc::CERTIFICATE);
                        b.leaf(vc::STATUS, status(&leaf, &chain, &roots, &crls, check_crl));
                        b.end();
                    }
                }
                (Err(error), _) | (_, Err(error)) => {
                    tracing::warn!(%error, "certificate validation configuration unavailable");
                    b.leaf(vc::STATUS, "17");
                }
            }
        }
    }
    b.end();
    (
        StatusCode::OK,
        [("Content-Type", "application/vnd.ms-sync.wbxml")],
        wbxml::encode_document(&b.finish()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/root.der");
    const CRL: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/list.der");
    const FUTURE_CRL: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/future.der");
    const GOOD: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/good.der");
    const REVOKED: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/revoked.der");
    const NO_SIGN: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/no-sign.der");
    const WRONG_EKU: &[u8] = include_bytes!("../../tests/fixtures/validate-cert/wrong-eku.der");

    #[test]
    fn validates_signing_certificates_and_revocation() {
        let roots = [ROOT.to_vec()];
        let crls = [CRL.to_vec()];
        assert_eq!(status(GOOD, &[], &roots, &crls, false), "1");
        assert_eq!(status(REVOKED, &[], &roots, &crls, false), "13");
        assert_eq!(status(REVOKED, &[], &roots, &crls, true), "13");
        assert_eq!(status(NO_SIGN, &[], &roots, &crls, false), "6");
        assert_eq!(status(WRONG_EKU, &[], &roots, &crls, false), "6");
        assert_eq!(status(GOOD, &[], &roots, &[], false), "1");
        assert_eq!(status(GOOD, &[], &roots, &[], true), "16");
        assert_eq!(
            status(GOOD, &[], &roots, &[FUTURE_CRL.to_vec()], false),
            "1"
        );
        assert_eq!(
            status(GOOD, &[], &roots, &[FUTURE_CRL.to_vec()], true),
            "16"
        );
        assert_eq!(status(b"invalid", &[], &roots, &crls, false), "5");
        assert_eq!(
            status(GOOD, &[], &roots, &[b"invalid".to_vec()], false),
            "17"
        );
    }

    #[test]
    fn check_crl_defaults_to_false_and_accepts_one_boolean_value() {
        fn document(check_values: &[&str]) -> Document {
            let mut builder = DocumentBuilder::new();
            builder.start(vc::VALIDATE_CERT);
            builder.start(vc::CERTIFICATES);
            builder.leaf(vc::CERTIFICATE, STANDARD.encode(GOOD));
            builder.end();
            for value in check_values {
                builder.leaf(vc::CHECK_CRL, *value);
            }
            builder.end();
            builder.finish()
        }

        assert!(!request(&document(&[])).unwrap().check_crl);
        assert!(!request(&document(&["0"])).unwrap().check_crl);
        assert!(request(&document(&["1"])).unwrap().check_crl);
        assert!(request(&document(&["2"])).is_err());
        assert!(request(&document(&["0", "1"])).is_err());
    }
}
