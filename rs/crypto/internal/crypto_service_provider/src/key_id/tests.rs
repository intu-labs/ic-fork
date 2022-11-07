use crate::KeyId;
use ic_crypto_internal_threshold_sig_ecdsa::{EccCurveType, EccPoint, MEGaPublicKey};
use ic_crypto_internal_types::sign::threshold_sig::ni_dkg::ni_dkg_groth20_bls12_381::FsEncryptionPublicKey;

#[test]
fn should_fail_to_create_key_id_from_mega_key_with_unsupported_curve() {
    let mega_public_key = MEGaPublicKey::new(EccPoint::identity(EccCurveType::P256));
    assert_eq!(
        KeyId::try_from(&mega_public_key),
        Err("unsupported curve: P256".to_string())
    );
}

mod stability_tests {
    use super::*;
    use crate::CspPublicKey;
    use hex::FromHex;
    use ic_crypto_internal_test_vectors::ed25519::TESTVEC_MESSAGE_LEN_256_BIT_STABILITY_1_PK;
    use ic_crypto_internal_test_vectors::ed25519::TESTVEC_MESSAGE_LEN_256_BIT_STABILITY_2_PK;
    use ic_crypto_internal_test_vectors::ed25519::TESTVEC_RFC8032_ED25519_SHA_ABC_PK;
    use ic_crypto_internal_test_vectors::multi_bls12_381::TESTVEC_MULTI_BLS12_381_1_PK;
    use ic_crypto_internal_test_vectors::multi_bls12_381::TESTVEC_MULTI_BLS12_381_2_PK;
    use ic_crypto_internal_test_vectors::multi_bls12_381::TESTVEC_MULTI_BLS12_381_3_PK;
    use ic_crypto_internal_test_vectors::multi_bls12_381::TESTVEC_MULTI_BLS12_381_4_PK;
    use ic_crypto_internal_threshold_sig_ecdsa::PedersenCommitment;
    use ic_crypto_internal_threshold_sig_ecdsa::{PolynomialCommitment, SimpleCommitment};
    use ic_crypto_internal_types::curves::bls12_381;
    use ic_crypto_internal_types::encrypt::forward_secure::CspFsEncryptionPublicKey;
    use ic_crypto_internal_types::sign::threshold_sig::public_coefficients::bls12_381::PublicCoefficientsBytes;
    use ic_crypto_internal_types::sign::threshold_sig::public_coefficients::CspPublicCoefficients;
    use ic_crypto_internal_types::sign::threshold_sig::public_key::bls12_381::PublicKeyBytes;
    use ic_crypto_tls_interfaces::TlsPublicKeyCert;
    use openssl::x509::X509;
    use std::fmt::Debug;

    #[derive(Debug)]
    struct ParameterizedTest<U, V> {
        input: U,
        expected: V,
    }

    impl<U, V: AsRef<[u8]>> ParameterizedTest<U, V> {
        fn expected_key_id(&self) -> KeyId {
            KeyId::from_hex(&self.expected).expect("invalid KeyId")
        }
    }

    #[test]
    fn should_provide_stable_string_value_from_hex() {
        let key_id =
            KeyId::from_hex("e1299603ca276e7164d25be3596f98c6139202959b6a83195acf0c5121d57742")
                .expect("invalid key id");
        let string_value = key_id.to_string();

        assert_eq!(
            string_value,
            "KeyId(0xe1299603ca276e7164d25be3596f98c6139202959b6a83195acf0c5121d57742)"
        )
    }

    #[test]
    fn should_provide_stable_string_value_from_bytes() {
        let key_id = KeyId::from([0u8; 32]);
        let string_value = key_id.to_string();
        assert_eq!(
            string_value,
            "KeyId(0x0000000000000000000000000000000000000000000000000000000000000000)"
        );

        let key_id = KeyId::from([1u8; 32]);
        let string_value = key_id.to_string();
        assert_eq!(
            string_value,
            "KeyId(0x0101010101010101010101010101010101010101010101010101010101010101)"
        )
    }

    #[test]
    fn should_instantiate_same_key_id_from_display_string() {
        let key_id =
            KeyId::from_hex("e1299603ca276e7164d25be3596f98c6139202959b6a83195acf0c5121d57742")
                .expect("invalid key id");

        let reconstructed_key_id =
            KeyId::try_from(key_id.to_string().as_str()).expect("invalid key id");

        assert_eq!(key_id, reconstructed_key_id);
    }

    #[test]
    fn should_provide_stable_key_id_from_display_string() {
        let displayed_key_id =
            "KeyId(0xd9564f1e7ab210c9f0c95d4627d5266485b4a7724048a36170c8ff5ac2915a48)";
        let computed_key_id = KeyId::try_from(displayed_key_id).expect("invalid key id");
        let expected_key_id =
            KeyId::from_hex("d9564f1e7ab210c9f0c95d4627d5266485b4a7724048a36170c8ff5ac2915a48")
                .expect("invalid key id");

        assert_eq!(computed_key_id, expected_key_id);
    }

    #[test]
    fn should_provide_stable_key_id_from_public_key_hash() {
        let tests = vec![
            ParameterizedTest {
                input: CspPublicKey::ed25519_from_hex(TESTVEC_RFC8032_ED25519_SHA_ABC_PK),
                expected: "d9564f1e7ab210c9f0c95d4627d5266485b4a7724048a36170c8ff5ac2915a48",
            },
            ParameterizedTest {
                input: CspPublicKey::ed25519_from_hex(TESTVEC_MESSAGE_LEN_256_BIT_STABILITY_1_PK),
                expected: "657b58570a2f72f6f24f9d574d766a57d323cbff06914ff70b8c54a0be60afc4",
            },
            ParameterizedTest {
                input: CspPublicKey::ed25519_from_hex(TESTVEC_MESSAGE_LEN_256_BIT_STABILITY_2_PK),
                expected: "1566296d90371b5273ec084fbdfeb80d06036bb9556657dacff522670ada424e",
            },
            ParameterizedTest {
                input: CspPublicKey::multi_bls12381_from_hex(TESTVEC_MULTI_BLS12_381_1_PK),
                expected: "bf7002780d49b0d397873f1638bbc7adb9f0b44071561a040b39291b92325875",
            },
            ParameterizedTest {
                input: CspPublicKey::multi_bls12381_from_hex(TESTVEC_MULTI_BLS12_381_2_PK),
                expected: "db832fa83c8b613abe4706dfde8f6cf39cba706c37223ac617666b869bf00405",
            },
            ParameterizedTest {
                input: CspPublicKey::multi_bls12381_from_hex(TESTVEC_MULTI_BLS12_381_3_PK),
                expected: "7b96cd3c54b615ae95d4862bfafbb17c5771ff3949b5eacb8fab53ae363b68e3",
            },
            ParameterizedTest {
                input: CspPublicKey::multi_bls12381_from_hex(TESTVEC_MULTI_BLS12_381_4_PK),
                expected: "e1299603ca276e7164d25be3596f98c6139202959b6a83195acf0c5121d57742",
            },
        ];

        for test in &tests {
            assert_eq!(
                KeyId::from(&test.input),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            );
        }
    }

    #[test]
    fn should_provide_stable_key_id_from_mega_key() {
        let tests = vec![
            ParameterizedTest {
                input: MEGaPublicKey::new(EccPoint::identity(EccCurveType::K256)),
                expected: "ea1004285ebbadc58afc93ca583973c793e1ee5c9cefa7d0165491f19937c1ed",
            },
            ParameterizedTest {
                input: MEGaPublicKey::new(
                    EccPoint::generator_g(EccCurveType::K256).expect("error retrieving generator"),
                ),
                expected: "4aeda75e42b4ca12c3d278a4684849bccbfd3ed6861d16fbee6c2585e7560039",
            },
            ParameterizedTest {
                input: MEGaPublicKey::new(
                    EccPoint::generator_h(EccCurveType::K256).expect("error retrieving generator"),
                ),
                expected: "502da182fa4451163418bb07073182ca280aa4fb1f652b70f5b3b8f1642579cb",
            },
        ];
        for test in &tests {
            assert_eq!(
                KeyId::try_from(&test.input).expect("invalid KeyId"),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            );
        }
    }

    #[test]
    fn should_provide_stable_key_id_from_forward_secure_key() {
        let tests = vec![
            ParameterizedTest {
                input: csp_fs_enc_pk(42),
                expected: "f3d2dd180ea9710063eeb6f52d838c712d585ad3de472316375ab876315faaf2",
            },
            ParameterizedTest {
                input: csp_fs_enc_pk(43),
                expected: "ecc7a325ebfd5b603036f30db773f652397376a029005b16fbaa1aa496558abe",
            },
            ParameterizedTest {
                input: csp_fs_enc_pk(44),
                expected: "f4c8f1604983d588e37b122651cf6f4e296c99875ed1291b3a54d2850dea3db7",
            },
        ];
        for test in &tests {
            assert_eq!(
                KeyId::from(&test.input),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            );
        }
    }

    #[test]
    fn should_provide_stable_key_id_from_commitment() {
        let generator_g_k256 =
            EccPoint::generator_g(EccCurveType::K256).expect("error retrieving generator");
        let generator_h_k256 =
            EccPoint::generator_h(EccCurveType::K256).expect("error retrieving generator");

        let generator_g_p256 =
            EccPoint::generator_g(EccCurveType::P256).expect("error retrieving generator");
        let generator_h_p256 =
            EccPoint::generator_h(EccCurveType::P256).expect("error retrieving generator");
        let tests = vec![
            ParameterizedTest {
                input: PolynomialCommitment::Simple(SimpleCommitment {
                    points: vec![generator_g_k256.clone(), generator_h_k256.clone()],
                }),
                expected: "317266bb4c9a48e402c80df3908872d78514e20ed277c50e32608b1a0b4b8803",
            },
            ParameterizedTest {
                input: PolynomialCommitment::Simple(SimpleCommitment {
                    points: vec![generator_g_p256.clone(), generator_h_p256.clone()],
                }),
                expected: "c8be99e090993026ff60d32f4424f436f3051020cec9a638a47a7db9619e679f",
            },
            ParameterizedTest {
                input: PolynomialCommitment::Pedersen(PedersenCommitment {
                    points: vec![generator_g_k256, generator_h_k256],
                }),
                expected: "e490f204848d40835434944b5a5ee4c9d2ae2c7dc8ea4af8bf66f790f3ee87a2",
            },
            ParameterizedTest {
                input: PolynomialCommitment::Pedersen(PedersenCommitment {
                    points: vec![generator_g_p256, generator_h_p256],
                }),
                expected: "a1211fbc604a231eccd0879b019aea8f1a055ace0d79fd08a78457bef1c01ef8",
            },
        ];

        for test in &tests {
            assert_eq!(
                KeyId::from(&test.input),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            );
        }
    }

    #[test]
    fn should_provide_stable_key_id_from_tls_cert() {
        let docs_rs_cert = r#"-----BEGIN CERTIFICATE-----
MIIFxjCCBK6gAwIBAgIQDF7pmq7PPvZyjj98sQshDzANBgkqhkiG9w0BAQsFADBG
MQswCQYDVQQGEwJVUzEPMA0GA1UEChMGQW1hem9uMRUwEwYDVQQLEwxTZXJ2ZXIg
Q0EgMUIxDzANBgNVBAMTBkFtYXpvbjAeFw0yMjAxMDYwMDAwMDBaFw0yMzAyMDQy
MzU5NTlaMBIxEDAOBgNVBAMTB2RvY3MucnMwggEiMA0GCSqGSIb3DQEBAQUAA4IB
DwAwggEKAoIBAQCW2k7u1nH0SK7/cUXUQi8/6wCsb4/4AYaGviyUuc8AMQ/7b/d3
ZcC9tcB4a7D3PjGF1lqsCxA0PqSa/GW3bhB9U2lwpNsFd5gQMDbsbZ+fNHF8aI+Y
HgAJ40XPLV07VMhegSyNYAZWDu4lN9/XPSwKbQ+nYzVp5DBpkC8IuDnUcoCgAxKF
l5+ZwZ/PS9Fvix9hjBA5KmmFDXODM4ivHEmZ584yq4NP6RkfkjeTTGhXvTmJ79LV
4xWM7pWPlCPfENadQSW1J0Gs3E5c7s9TUXFq5d9z11Kssy2RmdLeq+z55sNdx5s/
7wrs1i7pzLN6sc6BCxMLBxJ510g/DrZFwfSbAgMBAAGjggLiMIIC3jAfBgNVHSME
GDAWgBRZpGYGUqB7lZI8o5QHJ5Z0W/k90DAdBgNVHQ4EFgQUQKgdLtwe5AMsKDN5
S5kz0QI5ibcwEgYDVR0RBAswCYIHZG9jcy5yczAOBgNVHQ8BAf8EBAMCBaAwHQYD
VR0lBBYwFAYIKwYBBQUHAwEGCCsGAQUFBwMCMD0GA1UdHwQ2MDQwMqAwoC6GLGh0
dHA6Ly9jcmwuc2NhMWIuYW1hem9udHJ1c3QuY29tL3NjYTFiLTEuY3JsMBMGA1Ud
IAQMMAowCAYGZ4EMAQIBMHUGCCsGAQUFBwEBBGkwZzAtBggrBgEFBQcwAYYhaHR0
cDovL29jc3Auc2NhMWIuYW1hem9udHJ1c3QuY29tMDYGCCsGAQUFBzAChipodHRw
Oi8vY3J0LnNjYTFiLmFtYXpvbnRydXN0LmNvbS9zY2ExYi5jcnQwDAYDVR0TAQH/
BAIwADCCAX4GCisGAQQB1nkCBAIEggFuBIIBagFoAHcA6D7Q2j71BjUy51covIlr
yQPTy9ERa+zraeF3fW0GvW4AAAF+LUyddwAABAMASDBGAiEAs6bwaF8J8ykU2OqR
m8GwkPGNtA6JIe7yz9pTIu30yjYCIQDRMU6Ae9H2/zXkItJ538iPvsqDX2trKtlO
OgBXPAySugB2ADXPGRu/sWxXvw+tTG1Cy7u2JyAmUeo/4SrvqAPDO9ZMAAABfi1M
nXQAAAQDAEcwRQIhAJHOl+EyCqMRSplGDQVobeSXizm0hlAOyR6Ba1v/ntyzAiAC
/4EW4h/cL6aWABaFnyOOSCHT8NydEyBzk/Y5+w9tpgB1ALNzdwfhhFD4Y4bWBanc
EQlKeS2xZwwLh9zwAw55NqWaAAABfi1MnasAAAQDAEYwRAIgfXZYrSV4w8S5Kwim
+clHZLh8nMwdU9d3G47qHxI1sJcCIH+GHWe32JsqKi0dwEjiQ7/LhAMfznD47bcF
i/ZXNoBBMA0GCSqGSIb3DQEBCwUAA4IBAQBnamHdviwVXKfuLpmvV3FOqUPwUxoo
65v3T0+0AasxSIruWv0JLftB7anCVS/phchB6ZWOVrvv1gOfWQ7p7mTvx3AMQHHi
mo+Gw/VbrZU8zdkEE3iNhSHYg5szS/nwZYiYcLnHI4PlZV26op7Fu/ufLPOrcm42
44UZIihaWJX9zDLi/guVmxBgbVTvGMJdq4FXuztFMApaj9JJ2Gh0zvbBtBpij0Eu
t7Ica9iKR8XXVy+W5eyW52YYPbGzXZ0FgxPcOMk3Tm2qx/zJJ7pkN+rJeIEgQHEj
2nMxM1gYvf7AKqhkVEejCTS4APko/O87gdXnc4uPV0s+YZk3YLXd95t/
-----END CERTIFICATE-----"#;

        let tests = vec![ParameterizedTest {
            input: tls_public_key_cert_from_pem(docs_rs_cert),
            expected: "589e6e2741aef52ae6dd57cd2101d3f1537bff00ccc4a82f340db7a94a232386",
        }];

        for test in &tests {
            assert_eq!(
                KeyId::from(&test.input),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            );
        }
    }

    #[test]
    fn should_provide_stable_key_id_from_public_coefficients() {
        let tests = vec![
            ParameterizedTest {
                input: csp_public_coefficients(
                    "9772c16106e9c70b2073dfe17989225d\
                d10f3adb675365fc6d833587ad4cbd3a\
                e692ad1e20679003f676b0b089e83feb\
                058b3e8b9fc9552e30787cb4a541a1c3\
                bf67a02e91fc648b2c19f4bb333e14c5\
                c73b9bfbc5ec56dadabb07ff15d45124",
                ),
                expected: "158626c7c78741000e9ab35970ff887c63fbc8596e9e40cb32472b67150be96d",
            },
            ParameterizedTest {
                input: csp_public_coefficients(TESTVEC_MULTI_BLS12_381_1_PK),
                expected: "b2174971f382200287319ee1680088c917a019cb9b1469105c3a5e42459844a3",
            },
            ParameterizedTest {
                input: csp_public_coefficients(TESTVEC_MULTI_BLS12_381_2_PK),
                expected: "b82b7a16e60e1b8a643eaccb79b192cfe047d32c85a8f757cdbf68d3e910d64f",
            },
            ParameterizedTest {
                input: csp_public_coefficients(TESTVEC_MULTI_BLS12_381_3_PK),
                expected: "3239d711728ed30d26a17f68523dec7e86b2496af00ae672733a7d245d5915a6",
            },
            ParameterizedTest {
                input: csp_public_coefficients(TESTVEC_MULTI_BLS12_381_4_PK),
                expected: "8df4243f903775f7b4c626c2e5554f0251baf69ab091cb7ce866b724b9eb4c2d",
            },
        ];

        for test in &tests {
            assert_eq!(
                KeyId::from(&test.input),
                test.expected_key_id(),
                "Parameterized test {:?} failed",
                &test
            )
        }
    }

    fn tls_public_key_cert_from_pem(pem_cert: &str) -> TlsPublicKeyCert {
        TlsPublicKeyCert::new_from_x509(
            X509::from_pem(pem_cert.as_bytes()).expect("error parsing X509"),
        )
        .expect("error parsing certificate")
    }

    fn csp_public_coefficients<T: AsRef<[u8]>>(public_key: T) -> CspPublicCoefficients {
        let raw_public_key = hex_to_bytes(public_key);
        CspPublicCoefficients::Bls12_381(PublicCoefficientsBytes {
            coefficients: vec![PublicKeyBytes(raw_public_key)],
        })
    }

    fn hex_to_bytes<T: AsRef<[u8]>, const N: usize>(data: T) -> [u8; N] {
        hex::decode(data)
            .expect("error decoding hex")
            .try_into()
            .expect("wrong size of array")
    }

    pub fn csp_fs_enc_pk(data: u8) -> CspFsEncryptionPublicKey {
        CspFsEncryptionPublicKey::Groth20_Bls12_381(FsEncryptionPublicKey(bls12_381::G1(
            [data; bls12_381::G1::SIZE],
        )))
    }
}
