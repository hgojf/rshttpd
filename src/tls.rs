use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::certs;
use std::io::BufReader;

pub fn certs_from_file(file: std::fs::File) -> Result<Vec<CertificateDer<'static>>, std::io::Error> {
    let mut reader = BufReader::new(file);
    let certs = certs(&mut reader);
	let certs: Vec<_> = certs.map(|cert| cert)
		.collect::<Result<Vec<CertificateDer>, _>>()?;
    Ok(certs)
}

pub fn keys_from_file(file: std::fs::File) -> Result<PrivateKeyDer<'static>, std::io::Error> {
	let mut reader = BufReader::new(file);
	let mut keys = rustls_pemfile::pkcs8_private_keys(&mut reader);
	
	let key = keys.next()
		.unwrap()?;
//		.ok_or(std::io::ErrorKind::NotFound.into())?;
	Ok(key.into())
}
