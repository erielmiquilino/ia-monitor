//! Leitura de segredos guardados pelo `safeStorage` do Electron no Windows.
//!
//! É o esquema `os_crypt` do Chromium, em duas camadas: uma chave AES-256
//! sorteada uma vez e guardada no `Local State` **envelopada em DPAPI**, e os
//! valores cifrados com AES-256-GCM usando essa chave. Vale para o token do
//! Claude Desktop e para o cookie jar de qualquer app Electron.
//!
//! Nada aqui persiste segredo: tudo sai em `Zeroizing` e morre com o escopo.

use anyhow::{anyhow, bail, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use zeroize::Zeroizing;

/// Prefixo do `encrypted_key` no `Local State` (`kDPAPIKeyPrefix`).
const DPAPI_PREFIX: &[u8] = b"DPAPI";
/// Prefixo de todo valor cifrado pelo esquema (`kEncryptionVersionPrefix`).
const V10_PREFIX: &[u8] = b"v10";

/// Tira o envelope `DPAPI` e devolve o que vai para o `CryptUnprotectData`.
///
/// Separado da chamada Win32 para o recorte do prefixo ser testável em
/// qualquer plataforma — é justamente a parte que quebra silenciosamente se o
/// tamanho do prefixo mudar.
pub fn strip_dpapi_prefix(raw: &[u8]) -> Result<&[u8]> {
    if !raw.starts_with(DPAPI_PREFIX) {
        bail!("`os_crypt.encrypted_key` sem o prefixo DPAPI — formato desconhecido");
    }
    Ok(&raw[DPAPI_PREFIX.len()..])
}

/// Decifra um valor `v10` do `os_crypt`.
///
/// Layout, fixo desde que o esquema existe:
/// `["v10" (3) | nonce (12) | ciphertext (n) | tag GCM (16)]`, AAD vazio.
///
/// Prefixo diferente de `v10` é erro explícito, nunca tentativa de adivinhar:
/// o Chrome 127+ introduziu o `v20` (App-Bound Encryption), que exige um
/// serviço COM elevado e não se decifra por este caminho. Se o Claude Desktop
/// migrar para ele um dia, a mensagem diz isso em vez de devolver lixo.
pub fn decrypt_v10(key: &[u8], blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let minimo = V10_PREFIX.len() + NONCE_LEN + AES_256_GCM.tag_len();
    if blob.len() < minimo {
        bail!(
            "blob cifrado curto demais: {} bytes, mínimo {minimo}",
            blob.len()
        );
    }
    if !blob.starts_with(V10_PREFIX) {
        let visto = String::from_utf8_lossy(&blob[..3]).to_string();
        bail!("prefixo {visto:?} não é v10 — esquema de cifra não suportado");
    }

    let nonce: [u8; NONCE_LEN] = blob[V10_PREFIX.len()..V10_PREFIX.len() + NONCE_LEN]
        .try_into()
        .expect("fatia do tamanho do nonce");
    let chave = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| anyhow!("chave AES-256 inválida ({} bytes)", key.len()))?;

    // O `ring` espera ciphertext e tag no mesmo buffer, que é exatamente o
    // que sobra depois do nonce.
    let mut buffer = blob[V10_PREFIX.len() + NONCE_LEN..].to_vec();
    let claro = LessSafeKey::new(chave)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut buffer,
        )
        .map_err(|_| anyhow!("falha ao decifrar: tag GCM não confere"))?;

    Ok(Zeroizing::new(claro.to_vec()))
}

/// Desfaz o envelope DPAPI do usuário atual.
///
/// Vai direto na API do Windows em vez de trazer uma dependência inteira para
/// uma chamada, como em `scheduler::idle_seconds`.
#[cfg(windows)]
pub fn dpapi_unprotect(input: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptUnprotectData(
            p_data_in: *const DataBlob,
            ppsz_descr: *mut *mut u16,
            p_entropy: *const DataBlob,
            pv_reserved: *const core::ffi::c_void,
            p_prompt: *const core::ffi::c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
    }
    extern "system" {
        fn LocalFree(h: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
    }

    // `pb_data` não é const na API; a cópia também evita expor o buffer do
    // chamador a uma escrita acidental.
    let mut entrada = input.to_vec();
    let de = DataBlob {
        cb_data: entrada.len() as u32,
        pb_data: entrada.as_mut_ptr(),
    };
    let mut para = DataBlob {
        cb_data: 0,
        pb_data: core::ptr::null_mut(),
    };

    // SAFETY: `de` aponta para `entrada`, viva durante toda a chamada. Os
    // parâmetros opcionais (descrição, entropia, reservado, prompt) são nulos,
    // que é o que a API espera quando nenhum deles é usado. `dw_flags = 0`
    // mantém o escopo de usuário — a chave do Chromium é protegida assim.
    let ok = unsafe {
        CryptUnprotectData(
            &de,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            &mut para,
        )
    };
    if ok == 0 {
        // SAFETY: leitura de um thread-local do Windows, sempre válida.
        let code = unsafe { GetLastError() };
        bail!(
            "CryptUnprotectData falhou (GetLastError={code}) — a chave pertence a outro usuário do Windows?"
        );
    }
    if para.pb_data.is_null() {
        bail!("CryptUnprotectData devolveu sucesso com buffer nulo");
    }

    // SAFETY: em sucesso, `pb_data`/`cb_data` descrevem um buffer válido
    // alocado pela própria API. Copiamos antes de liberar e não guardamos o
    // ponteiro.
    let saida = unsafe {
        let fatia = core::slice::from_raw_parts(para.pb_data, para.cb_data as usize);
        let copia = Zeroizing::new(fatia.to_vec());
        LocalFree(para.pb_data as *mut core::ffi::c_void);
        copia
    };
    Ok(saida)
}

#[cfg(not(windows))]
pub fn dpapi_unprotect(_input: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    bail!("DPAPI só existe no Windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::aead::BoundKey;

    /// Monta um blob no formato do Chromium, para os testes não dependerem de
    /// um arquivo real do Claude Desktop.
    fn cifra(key: &[u8; 32], nonce: [u8; NONCE_LEN], claro: &[u8]) -> Vec<u8> {
        struct Fixo([u8; NONCE_LEN], bool);
        impl ring::aead::NonceSequence for Fixo {
            fn advance(&mut self) -> std::result::Result<Nonce, ring::error::Unspecified> {
                if self.1 {
                    return Err(ring::error::Unspecified);
                }
                self.1 = true;
                Ok(Nonce::assume_unique_for_key(self.0))
            }
        }
        let mut selante = ring::aead::SealingKey::new(
            UnboundKey::new(&AES_256_GCM, key).unwrap(),
            Fixo(nonce, false),
        );
        let mut buffer = claro.to_vec();
        selante
            .seal_in_place_append_tag(Aad::empty(), &mut buffer)
            .unwrap();

        let mut blob = V10_PREFIX.to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&buffer);
        blob
    }

    const CHAVE: [u8; 32] = [7u8; 32];
    const NONCE: [u8; NONCE_LEN] = [3u8; NONCE_LEN];

    #[test]
    fn decifra_um_blob_v10_bem_formado() {
        let claro_original = br#"{"accessToken":"sk-ant-exemplo"}"#;
        let blob = cifra(&CHAVE, NONCE, claro_original);
        let claro = decrypt_v10(&CHAVE, &blob).expect("decifrar");
        assert_eq!(&claro[..], claro_original);
    }

    /// A tag do GCM é o que separa "decifrou" de "produziu bytes". Um bit
    /// trocado precisa virar erro, nunca plaintext parcial.
    #[test]
    fn tag_adulterada_nao_devolve_nada() {
        let mut blob = cifra(&CHAVE, NONCE, b"segredo");
        let ultimo = blob.len() - 1;
        blob[ultimo] ^= 0x01;
        assert!(decrypt_v10(&CHAVE, &blob).is_err());
    }

    #[test]
    fn chave_errada_nao_decifra() {
        let blob = cifra(&CHAVE, NONCE, b"segredo");
        assert!(decrypt_v10(&[9u8; 32], &blob).is_err());
    }

    /// O `v20` do Chrome 127+ exige um serviço COM elevado. Falhar com nome
    /// próprio é o que evita um bug de "decifra e devolve lixo" no dia em que
    /// o Claude Desktop migrar.
    #[test]
    fn prefixo_desconhecido_falha_com_nome() {
        let mut blob = cifra(&CHAVE, NONCE, b"segredo");
        blob[..3].copy_from_slice(b"v20");
        let erro = decrypt_v10(&CHAVE, &blob).unwrap_err().to_string();
        assert!(erro.contains("v20"), "o erro precisa dizer o que viu: {erro}");
    }

    #[test]
    fn blob_curto_nao_entra_em_panico() {
        for n in 0..31 {
            assert!(decrypt_v10(&CHAVE, &vec![0u8; n]).is_err(), "n={n}");
        }
    }

    #[test]
    fn recorta_o_prefixo_dpapi() {
        let mut raw = b"DPAPI".to_vec();
        raw.extend_from_slice(&[1, 2, 3]);
        assert_eq!(strip_dpapi_prefix(&raw).unwrap(), &[1, 2, 3]);
    }

    #[test]
    fn sem_prefixo_dpapi_e_erro() {
        assert!(strip_dpapi_prefix(b"XXXXX123").is_err());
        assert!(strip_dpapi_prefix(b"").is_err());
    }
}
