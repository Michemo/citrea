use core::fmt;
use core::result::Result::Ok;
use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::anyhow;
use bitcoin::absolute::LockTime;
use bitcoin::blockdata::opcodes::all::{OP_CHECKSIG, OP_ENDIF, OP_IF};
use bitcoin::blockdata::opcodes::OP_FALSE;
use bitcoin::blockdata::script;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::key::{TapTweak, TweakedPublicKey, UntweakedKeypair};
use bitcoin::script::PushBytesBuf;
use bitcoin::secp256k1::constants::SCHNORR_SIGNATURE_SIZE;
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::{self, Secp256k1, SecretKey, XOnlyPublicKey};
use bitcoin::sighash::{Prevouts, SighashCache};
use bitcoin::taproot::{ControlBlock, LeafVersion, TapLeafHash, TaprootBuilder};
use bitcoin::{
    Address, Amount, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid,
    Witness,
};
use tracing::{instrument, trace, warn};

use crate::helpers::{BODY_TAG, PUBLICKEY_TAG, RANDOM_TAG, ROLLUP_NAME_TAG, SIGNATURE_TAG};
use crate::spec::utxo::UTXO;
use crate::REVEAL_OUTPUT_AMOUNT;

// Signs a message with a private key
pub fn sign_blob_with_private_key(
    blob: &[u8],
    private_key: &SecretKey,
) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let message = sha256d::Hash::hash(blob).to_byte_array();
    let secp = Secp256k1::new();
    let public_key = secp256k1::PublicKey::from_secret_key(&secp, private_key);
    let msg = secp256k1::Message::from_digest_slice(&message).unwrap();
    let sig = secp.sign_ecdsa(&msg, private_key);
    Ok((
        sig.serialize_compact().to_vec(),
        public_key.serialize().to_vec(),
    ))
}

fn get_size(
    inputs: &[TxIn],
    outputs: &[TxOut],
    script: Option<&ScriptBuf>,
    control_block: Option<&ControlBlock>,
) -> usize {
    let mut tx = Transaction {
        input: inputs.to_owned(),
        output: outputs.to_owned(),
        lock_time: LockTime::ZERO,
        version: bitcoin::transaction::Version(2),
    };

    for i in 0..tx.input.len() {
        tx.input[i].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );
    }

    #[allow(clippy::unnecessary_unwrap)]
    if tx.input.len() == 1 && script.is_some() && control_block.is_some() {
        tx.input[0].witness.push(script.unwrap());
        tx.input[0].witness.push(control_block.unwrap().serialize());
    }

    tx.vsize()
}

fn choose_utxos(
    required_utxo: Option<UTXO>,
    utxos: &[UTXO],
    mut amount: u64,
) -> Result<(Vec<UTXO>, u64), anyhow::Error> {
    let mut chosen_utxos = vec![];
    let mut sum = 0;

    // First include a required utxo
    if let Some(required) = required_utxo {
        let req_amount = required.amount;
        chosen_utxos.push(required);
        sum += req_amount;
    }
    if sum >= amount {
        return Ok((chosen_utxos, sum));
    } else {
        amount -= sum;
    }

    let mut bigger_utxos: Vec<&UTXO> = utxos.iter().filter(|utxo| utxo.amount >= amount).collect();

    if !bigger_utxos.is_empty() {
        // sort vec by amount (small first)
        bigger_utxos.sort_by(|a, b| a.amount.cmp(&b.amount));

        // single utxo will be enough
        // so return the transaction
        let utxo = bigger_utxos[0];
        sum += utxo.amount;
        chosen_utxos.push(utxo.clone());

        Ok((chosen_utxos, sum))
    } else {
        let mut smaller_utxos: Vec<&UTXO> =
            utxos.iter().filter(|utxo| utxo.amount < amount).collect();

        // sort vec by amount (large first)
        smaller_utxos.sort_by(|a, b| b.amount.cmp(&a.amount));

        for utxo in smaller_utxos {
            sum += utxo.amount;
            chosen_utxos.push(utxo.clone());

            if sum >= amount {
                break;
            }
        }

        if sum < amount {
            return Err(anyhow!("not enough UTXOs"));
        }

        Ok((chosen_utxos, sum))
    }
}

#[instrument(level = "trace", skip(utxos), err)]
fn build_commit_transaction(
    prev_tx: Option<TxWithId>, // reuse outputs to add commit tx order
    mut utxos: Vec<UTXO>,
    recipient: Address,
    change_address: Address,
    output_value: u64,
    fee_rate: f64,
) -> Result<Transaction, anyhow::Error> {
    // get single input single output transaction size
    let size = get_size(
        &[TxIn {
            previous_output: OutPoint {
                txid: Txid::from_byte_array([0; 32]),
                vout: 0,
            },
            script_sig: script::Builder::new().into_script(),
            witness: Witness::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        }],
        &[TxOut {
            script_pubkey: recipient.clone().script_pubkey(),
            value: Amount::from_sat(output_value),
        }],
        None,
        None,
    );

    // fields other then tx_id, vout, script_pubkey and amount are not really important.
    let required_utxo = prev_tx.map(|tx| UTXO {
        tx_id: tx.id,
        vout: 0,
        script_pubkey: tx.tx.output[0].script_pubkey.to_hex_string(),
        address: None,
        amount: tx.tx.output[0].value.to_sat(),
        confirmations: 0,
        spendable: true,
        solvable: true,
    });

    if let Some(req_utxo) = &required_utxo {
        // if we don't do this, then we might end up using the required utxo twice
        // which would yield an invalid transaction
        // however using a different txo from the same tx is fine.
        utxos.retain(|utxo| !(utxo.vout == req_utxo.vout && utxo.tx_id == req_utxo.tx_id));
    }

    let mut iteration = 0;
    let mut last_size = size;

    let tx = loop {
        if iteration % 10 == 0 {
            trace!(iteration, "Trying to find commitment size");
            if iteration > 100 {
                warn!("Too many iterations choosing UTXOs");
            }
        }
        let fee = ((last_size as f64) * fee_rate).ceil() as u64;

        let input_total = output_value + fee;

        let (chosen_utxos, sum) = choose_utxos(required_utxo.clone(), &utxos, input_total)?;
        let has_change = (sum - input_total) >= REVEAL_OUTPUT_AMOUNT;
        let direct_return = !has_change;

        let outputs = if !has_change {
            vec![TxOut {
                value: Amount::from_sat(output_value),
                script_pubkey: recipient.script_pubkey(),
            }]
        } else {
            vec![
                TxOut {
                    value: Amount::from_sat(output_value),
                    script_pubkey: recipient.script_pubkey(),
                },
                TxOut {
                    value: Amount::from_sat(sum - input_total),
                    script_pubkey: change_address.script_pubkey(),
                },
            ]
        };

        let inputs: Vec<_> = chosen_utxos
            .iter()
            .map(|u| TxIn {
                previous_output: OutPoint {
                    txid: u.tx_id,
                    vout: u.vout,
                },
                script_sig: script::Builder::new().into_script(),
                witness: Witness::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            })
            .collect();

        if direct_return {
            break Transaction {
                lock_time: LockTime::ZERO,
                version: bitcoin::transaction::Version(2),
                input: inputs,
                output: outputs,
            };
        }

        let size = get_size(&inputs, &outputs, None, None);

        if size == last_size {
            break Transaction {
                lock_time: LockTime::ZERO,
                version: bitcoin::transaction::Version(2),
                input: inputs,
                output: outputs,
            };
        }

        last_size = size;
        iteration += 1;
    };

    Ok(tx)
}

#[allow(clippy::too_many_arguments)]
fn build_reveal_transaction(
    input_utxo: TxOut,
    input_txid: Txid,
    input_vout: u32,
    recipient: Address,
    output_value: u64,
    fee_rate: f64,
    reveal_script: &ScriptBuf,
    control_block: &ControlBlock,
) -> Result<Transaction, anyhow::Error> {
    let outputs: Vec<TxOut> = vec![TxOut {
        value: Amount::from_sat(output_value),
        script_pubkey: recipient.script_pubkey(),
    }];

    let inputs = vec![TxIn {
        previous_output: OutPoint {
            txid: input_txid,
            vout: input_vout,
        },
        script_sig: script::Builder::new().into_script(),
        witness: Witness::new(),
        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
    }];

    let size = get_size(&inputs, &outputs, Some(reveal_script), Some(control_block));

    let fee = ((size as f64) * fee_rate).ceil() as u64;

    let input_total = output_value + fee;

    if input_utxo.value < Amount::from_sat(REVEAL_OUTPUT_AMOUNT)
        || input_utxo.value < Amount::from_sat(input_total)
    {
        return Err(anyhow::anyhow!("input UTXO not big enough"));
    }

    let tx = Transaction {
        lock_time: LockTime::ZERO,
        version: bitcoin::transaction::Version(2),
        input: inputs,
        output: outputs,
    };

    Ok(tx)
}

/// Both transaction and its hash
#[derive(Clone)]
pub struct TxWithId {
    /// ID (hash)
    pub id: Txid,
    /// Transaction
    pub tx: Transaction,
}

impl fmt::Debug for TxWithId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxWithId")
            .field("id", &self.id)
            .field("tx", &"...")
            .finish()
    }
}

// TODO: parametrize hardness
// so tests are easier
// Creates the inscription transactions (commit and reveal)
#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace", skip_all, err)]
pub fn create_inscription_transactions(
    rollup_name: &str,
    body: Vec<u8>,
    signature: Vec<u8>,
    sequencer_public_key: Vec<u8>,
    prev_tx: Option<TxWithId>,
    utxos: Vec<UTXO>,
    recipient: Address,
    reveal_value: u64,
    commit_fee_rate: f64,
    reveal_fee_rate: f64,
    network: Network,
    reveal_tx_prefix: &[u8],
) -> Result<(Transaction, TxWithId), anyhow::Error> {
    // Create commit key
    let secp256k1 = Secp256k1::new();
    let key_pair = UntweakedKeypair::new(&secp256k1, &mut rand::thread_rng());
    let (public_key, _parity) = XOnlyPublicKey::from_keypair(&key_pair);

    // start creating inscription content
    let reveal_script_builder = script::Builder::new()
        .push_x_only_key(&public_key)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_FALSE)
        .push_opcode(OP_IF)
        .push_slice(PushBytesBuf::from(ROLLUP_NAME_TAG))
        .push_slice(
            PushBytesBuf::try_from(rollup_name.as_bytes().to_vec())
                .expect("Cannot push rollup name"),
        )
        .push_slice(PushBytesBuf::from(SIGNATURE_TAG))
        .push_slice(PushBytesBuf::try_from(signature).expect("Cannot push signature"))
        .push_slice(PushBytesBuf::from(PUBLICKEY_TAG))
        .push_slice(
            PushBytesBuf::try_from(sequencer_public_key).expect("Cannot push sequencer public key"),
        )
        .push_slice(PushBytesBuf::from(RANDOM_TAG));
    // This envelope is not finished yet. The random number will be added later and followed by the body

    // Start loop to find a 'nonce' i.e. random number that makes the reveal tx hash starting with zeros given length
    let mut nonce: i64 = 0;
    loop {
        if nonce % 10000 == 0 {
            trace!(nonce, "Trying to find commit & reveal nonce");
            if nonce > 65536 {
                warn!("Too many iterations finding nonce");
            }
        }
        let utxos = utxos.clone();
        let recipient = recipient.clone();
        // ownerships are moved to the loop
        let mut reveal_script_builder = reveal_script_builder.clone();

        // push first random number and body tag
        reveal_script_builder = reveal_script_builder
            .push_int(nonce)
            .push_slice(PushBytesBuf::from(BODY_TAG));

        // push body in chunks of 520 bytes
        for chunk in body.chunks(520) {
            reveal_script_builder = reveal_script_builder.push_slice(
                PushBytesBuf::try_from(chunk.to_vec()).expect("Cannot push body chunk"),
            );
        }
        // push end if
        reveal_script_builder = reveal_script_builder.push_opcode(OP_ENDIF);

        // finalize reveal script
        let reveal_script = reveal_script_builder.into_script();

        // create spend info for tapscript
        let taproot_spend_info = TaprootBuilder::new()
            .add_leaf(0, reveal_script.clone())
            .expect("Cannot add reveal script to taptree")
            .finalize(&secp256k1, public_key)
            .expect("Cannot finalize taptree");

        // create control block for tapscript
        let control_block = taproot_spend_info
            .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
            .expect("Cannot create control block");

        // create commit tx address
        let commit_tx_address = Address::p2tr(
            &secp256k1,
            public_key,
            taproot_spend_info.merkle_root(),
            network,
        );

        let commit_value = (get_size(
            &[TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([0; 32]),
                    vout: 0,
                },
                script_sig: script::Builder::new().into_script(),
                witness: Witness::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            }],
            &[TxOut {
                script_pubkey: recipient.clone().script_pubkey(),
                value: Amount::from_sat(reveal_value),
            }],
            Some(&reveal_script),
            Some(&control_block),
        ) as f64
            * reveal_fee_rate
            + reveal_value as f64)
            .ceil() as u64;

        // build commit tx
        let unsigned_commit_tx = build_commit_transaction(
            prev_tx.clone(),
            utxos,
            commit_tx_address.clone(),
            recipient.clone(),
            commit_value,
            commit_fee_rate,
        )?;

        let output_to_reveal = unsigned_commit_tx.output[0].clone();

        let mut reveal_tx = build_reveal_transaction(
            output_to_reveal.clone(),
            unsigned_commit_tx.compute_txid(),
            0,
            recipient,
            reveal_value,
            reveal_fee_rate,
            &reveal_script,
            &control_block,
        )?;

        let reveal_tx_id = reveal_tx.compute_txid();
        let reveal_hash = reveal_tx_id.as_raw_hash().to_byte_array();

        // check if first N bytes equal to the given prefix
        if reveal_hash.starts_with(reveal_tx_prefix) {
            // start signing reveal tx
            let mut sighash_cache = SighashCache::new(&mut reveal_tx);

            // create data to sign
            let signature_hash = sighash_cache
                .taproot_script_spend_signature_hash(
                    0,
                    &Prevouts::All(&[output_to_reveal]),
                    TapLeafHash::from_script(&reveal_script, LeafVersion::TapScript),
                    bitcoin::sighash::TapSighashType::Default,
                )
                .expect("Cannot create hash for signature");

            // sign reveal tx data
            let signature = secp256k1.sign_schnorr_with_rng(
                &secp256k1::Message::from_digest_slice(signature_hash.as_byte_array())
                    .expect("should be cryptographically secure hash"),
                &key_pair,
                &mut rand::thread_rng(),
            );

            // add signature to witness and finalize reveal tx
            let witness = sighash_cache.witness_mut(0).unwrap();
            witness.push(signature.as_ref());
            witness.push(reveal_script);
            witness.push(&control_block.serialize());

            // check if inscription locked to the correct address
            let recovery_key_pair =
                key_pair.tap_tweak(&secp256k1, taproot_spend_info.merkle_root());
            let (x_only_pub_key, _parity) = recovery_key_pair.to_inner().x_only_public_key();
            assert_eq!(
                Address::p2tr_tweaked(
                    TweakedPublicKey::dangerous_assume_tweaked(x_only_pub_key),
                    network,
                ),
                commit_tx_address
            );

            return Ok((
                unsigned_commit_tx,
                TxWithId {
                    id: reveal_tx_id,
                    tx: reveal_tx,
                },
            ));
        }

        nonce += 1;
    }
}

pub fn write_reveal_tx(tx: &[u8], tx_id: String) {
    let reveal_tx_file = File::create(format!("reveal_{}.tx", tx_id)).unwrap();
    let mut reveal_tx_writer = BufWriter::new(reveal_tx_file);
    reveal_tx_writer.write_all(tx).unwrap();
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::constants::SCHNORR_SIGNATURE_SIZE;
    use bitcoin::secp256k1::schnorr::Signature;
    use bitcoin::taproot::ControlBlock;
    use bitcoin::{Address, Amount, ScriptBuf, TxOut, Txid};

    use crate::helpers::compression::{compress_blob, decompress_blob};
    use crate::helpers::parsers::parse_transaction;
    use crate::spec::utxo::UTXO;
    use crate::REVEAL_OUTPUT_AMOUNT;

    #[test]
    fn compression_decompression() {
        let blob = std::fs::read("test_data/blob.txt").unwrap();

        // compress and measure time
        let time = std::time::Instant::now();
        let compressed_blob = compress_blob(&blob);
        println!("compression time: {:?}", time.elapsed());

        // decompress and measure time
        let time = std::time::Instant::now();
        let decompressed_blob = decompress_blob(&compressed_blob);
        println!("decompression time: {:?}", time.elapsed());

        assert_eq!(blob, decompressed_blob);

        // size
        println!("blob size: {}", blob.len());
        println!("compressed blob size: {}", compressed_blob.len());
        println!(
            "compression ratio: {}",
            (blob.len() as f64) / (compressed_blob.len() as f64)
        );
    }

    #[test]
    fn write_reveal_tx() {
        let tx = vec![100, 100, 100];
        let tx_id = "test_tx".to_string();

        super::write_reveal_tx(tx.as_slice(), tx_id);

        let file = std::fs::read("reveal_test_tx.tx").unwrap();

        assert_eq!(tx, file);

        std::fs::remove_file("reveal_test_tx.tx").unwrap();
    }

    #[allow(clippy::type_complexity)]
    fn get_mock_data() -> (&'static str, Vec<u8>, Vec<u8>, Vec<u8>, Address, Vec<UTXO>) {
        let rollup_name = "test_rollup";
        let body = vec![100; 1000];
        let signature = vec![100; 64];
        let sequencer_public_key = vec![100; 33];
        let address =
            Address::from_str("bc1pp8qru0ve43rw9xffmdd8pvveths3cx6a5t6mcr0xfn9cpxx2k24qf70xq9")
                .unwrap()
                .require_network(bitcoin::Network::Bitcoin)
                .unwrap();
        let utxos = vec![
            UTXO {
                tx_id: Txid::from_str(
                    "4cfbec13cf1510545f285cceceb6229bd7b6a918a8f6eba1dbee64d26226a3b7",
                )
                .unwrap(),
                vout: 0,
                address: Some(
                    Address::from_str(
                        "bc1pp8qru0ve43rw9xffmdd8pvveths3cx6a5t6mcr0xfn9cpxx2k24qf70xq9",
                    )
                    .unwrap(),
                ),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 1_000_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
            UTXO {
                tx_id: Txid::from_str(
                    "44990141674ff56ed6fee38879e497b2a726cddefd5e4d9b7bf1c4e561de4347",
                )
                .unwrap(),
                vout: 0,
                address: Some(
                    Address::from_str(
                        "bc1pp8qru0ve43rw9xffmdd8pvveths3cx6a5t6mcr0xfn9cpxx2k24qf70xq9",
                    )
                    .unwrap(),
                ),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 100_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
            UTXO {
                tx_id: Txid::from_str(
                    "4dbe3c10ee0d6bf16f9417c68b81e963b5bccef3924bbcb0885c9ea841912325",
                )
                .unwrap(),
                vout: 0,
                address: Some(
                    Address::from_str(
                        "bc1pp8qru0ve43rw9xffmdd8pvveths3cx6a5t6mcr0xfn9cpxx2k24qf70xq9",
                    )
                    .unwrap(),
                ),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 10_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
        ];

        (
            rollup_name,
            body,
            signature,
            sequencer_public_key,
            address,
            utxos,
        )
    }

    #[test]
    fn choose_utxos() {
        let (_, _, _, _, _, utxos) = get_mock_data();

        let (chosen_utxos, sum) = super::choose_utxos(None, &utxos, 105_000).unwrap();

        assert_eq!(sum, 1_000_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[0]);

        let (chosen_utxos, sum) = super::choose_utxos(None, &utxos, 1_005_000).unwrap();

        assert_eq!(sum, 1_100_000);
        assert_eq!(chosen_utxos.len(), 2);
        assert_eq!(chosen_utxos[0], utxos[0]);
        assert_eq!(chosen_utxos[1], utxos[1]);

        let (chosen_utxos, sum) = super::choose_utxos(None, &utxos, 100_000).unwrap();

        assert_eq!(sum, 100_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[1]);

        let (chosen_utxos, sum) = super::choose_utxos(None, &utxos, 90_000).unwrap();

        assert_eq!(sum, 100_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[1]);

        let res = super::choose_utxos(None, &utxos, 100_000_000);

        assert!(res.is_err());
        assert_eq!(format!("{}", res.unwrap_err()), "not enough UTXOs");
    }

    #[test]
    fn build_commit_transaction() {
        let (_, _, _, _, address, utxos) = get_mock_data();

        let recipient =
            Address::from_str("bc1p2e37kuhnsdc5zvc8zlj2hn6awv3ruavak6ayc8jvpyvus59j3mwqwdt0zc")
                .unwrap()
                .require_network(bitcoin::Network::Bitcoin)
                .unwrap();
        let mut tx = super::build_commit_transaction(
            None,
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            5_000,
            8.0,
        )
        .unwrap();

        tx.input[0].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );

        // 154 vB * 8 sat/vB = 1232 sats
        // 5_000 + 1232 = 6232
        // input: 10000
        // outputs: 5_000 + 3.768
        assert_eq!(tx.vsize(), 154);
        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.output.len(), 2);
        assert_eq!(tx.output[0].value, Amount::from_sat(5_000));
        assert_eq!(tx.output[0].script_pubkey, recipient.script_pubkey());
        assert_eq!(tx.output[1].value, Amount::from_sat(3_768));
        assert_eq!(tx.output[1].script_pubkey, address.script_pubkey());

        let mut tx = super::build_commit_transaction(
            None,
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            5_000,
            45.0,
        )
        .unwrap();

        tx.input[0].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );

        // 111 vB * 45 sat/vB = 4.995 sats
        // 5_000 + 4928 = 9995
        // input: 10000
        // outputs: 5_000
        assert_eq!(tx.vsize(), 111);
        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.output.len(), 1);
        assert_eq!(tx.output[0].value, Amount::from_sat(5_000));
        assert_eq!(tx.output[0].script_pubkey, recipient.script_pubkey());

        let mut tx = super::build_commit_transaction(
            None,
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            5_000,
            32.0,
        )
        .unwrap();

        tx.input[0].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );

        // you expect
        // 154 vB * 32 sat/vB = 4.928 sats
        // 5_000 + 4928 = 9928
        // input: 10000
        // outputs: 5_000 72
        // instead do
        // input: 10000
        // outputs: 5_000
        // so size is actually 111
        assert_eq!(tx.vsize(), 111);
        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.output.len(), 1);
        assert_eq!(tx.output[0].value, Amount::from_sat(5_000));
        assert_eq!(tx.output[0].script_pubkey, recipient.script_pubkey());

        let mut tx = super::build_commit_transaction(
            None,
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            1_050_000,
            5.0,
        )
        .unwrap();

        tx.input[0].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );
        tx.input[1].witness.push(
            Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
                .unwrap()
                .as_ref(),
        );

        // 212 vB * 5 sat/vB = 1060 sats
        // 1_050_000 + 1060 = 1_051_060
        // inputs: 1_000_000 100_000
        // outputs: 1_050_000 8940
        assert_eq!(tx.vsize(), 212);
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 2);
        assert_eq!(tx.output[0].value, Amount::from_sat(1_050_000));
        assert_eq!(tx.output[0].script_pubkey, recipient.script_pubkey());
        assert_eq!(tx.output[1].value, Amount::from_sat(48940));
        assert_eq!(tx.output[1].script_pubkey, address.script_pubkey());

        let prev_tx = tx;
        let prev_tx_id = prev_tx.compute_txid();
        let tx = super::build_commit_transaction(
            Some(super::TxWithId {
                id: prev_tx_id,
                tx: prev_tx.clone(),
            }),
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            100_000_000_000,
            32.0,
        );

        assert!(tx.is_err());
        assert_eq!(format!("{}", tx.unwrap_err()), "not enough UTXOs");

        let prev_utxos: Vec<UTXO> = prev_tx
            .output
            .iter()
            .enumerate()
            .map(|(i, o)| UTXO {
                tx_id: prev_tx_id,
                vout: i as u32,
                script_pubkey: o.script_pubkey.to_hex_string(),
                address: None,
                confirmations: 0,
                amount: o.value.to_sat(),
                spendable: true,
                solvable: true,
            })
            .collect();
        let prev_utxo = utxos.clone().into_iter().chain(prev_utxos).collect();

        let tx = super::build_commit_transaction(
            Some(super::TxWithId {
                id: prev_tx_id,
                tx: prev_tx,
            }),
            prev_utxo,
            recipient.clone(),
            address.clone(),
            50000,
            32.0,
        )
        .unwrap();

        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.input[0].previous_output.txid, prev_tx_id);

        let tx = super::build_commit_transaction(
            None,
            utxos.clone(),
            recipient.clone(),
            address.clone(),
            100_000_000_000,
            32.0,
        );

        assert!(tx.is_err());
        assert_eq!(format!("{}", tx.unwrap_err()), "not enough UTXOs");

        let tx = super::build_commit_transaction(
            None,
            vec![UTXO {
                tx_id: Txid::from_str(
                    "4cfbec13cf1510545f285cceceb6229bd7b6a918a8f6eba1dbee64d26226a3b7",
                )
                .unwrap(),
                vout: 0,
                address: Some(
                    Address::from_str(
                        "bc1pp8qru0ve43rw9xffmdd8pvveths3cx6a5t6mcr0xfn9cpxx2k24qf70xq9",
                    )
                    .unwrap(),
                ),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 152,
                confirmations: 100,
                spendable: true,
                solvable: true,
            }],
            recipient.clone(),
            address.clone(),
            100_000_000_000,
            32.0,
        );

        assert!(tx.is_err());
        assert_eq!(format!("{}", tx.unwrap_err()), "not enough UTXOs");
    }

    #[test]
    fn build_reveal_transaction() {
        let (_, _, _, _, address, utxos) = get_mock_data();

        let utxo = utxos.first().unwrap();
        let script = ScriptBuf::from_hex("62a58f2674fd840b6144bea2e63ebd35c16d7fd40252a2f28b2a01a648df356343e47976d7906a0e688bf5e134b6fd21bd365c016b57b1ace85cf30bf1206e27").unwrap();
        let control_block = ControlBlock::decode(&[
            193, 165, 246, 250, 6, 222, 28, 9, 130, 28, 217, 67, 171, 11, 229, 62, 48, 206, 219,
            111, 155, 208, 6, 7, 119, 63, 146, 90, 227, 254, 231, 232, 249,
        ])
        .unwrap(); // should be 33 bytes

        let mut tx = super::build_reveal_transaction(
            TxOut {
                value: Amount::from_sat(utxo.amount),
                script_pubkey: ScriptBuf::from_hex(utxo.script_pubkey.as_str()).unwrap(),
            },
            utxo.tx_id,
            utxo.vout,
            address.clone(),
            REVEAL_OUTPUT_AMOUNT,
            8.0,
            &script,
            &control_block,
        )
        .unwrap();

        tx.input[0].witness.push([0; SCHNORR_SIGNATURE_SIZE]);
        tx.input[0].witness.push(script.clone());
        tx.input[0].witness.push(control_block.serialize());

        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.input[0].previous_output.txid, utxo.tx_id);
        assert_eq!(tx.input[0].previous_output.vout, utxo.vout);

        assert_eq!(tx.output.len(), 1);
        assert_eq!(tx.output[0].value, Amount::from_sat(REVEAL_OUTPUT_AMOUNT));
        assert_eq!(tx.output[0].script_pubkey, address.script_pubkey());

        let utxo = utxos.get(2).unwrap();

        let tx = super::build_reveal_transaction(
            TxOut {
                value: Amount::from_sat(utxo.amount),
                script_pubkey: ScriptBuf::from_hex(utxo.script_pubkey.as_str()).unwrap(),
            },
            utxo.tx_id,
            utxo.vout,
            address.clone(),
            REVEAL_OUTPUT_AMOUNT,
            75.0,
            &script,
            &control_block,
        );

        assert!(tx.is_err());
        assert_eq!(format!("{}", tx.unwrap_err()), "input UTXO not big enough");

        let utxo = utxos.get(2).unwrap();

        let tx = super::build_reveal_transaction(
            TxOut {
                value: Amount::from_sat(utxo.amount),
                script_pubkey: ScriptBuf::from_hex(utxo.script_pubkey.as_str()).unwrap(),
            },
            utxo.tx_id,
            utxo.vout,
            address.clone(),
            9999,
            1.0,
            &script,
            &control_block,
        );

        assert!(tx.is_err());
        assert_eq!(format!("{}", tx.unwrap_err()), "input UTXO not big enough");
    }
    #[test]
    fn create_inscription_transactions() {
        let (rollup_name, body, signature, sequencer_public_key, address, utxos) = get_mock_data();

        let tx_prefix = &[0u8];
        let (commit, reveal) = super::create_inscription_transactions(
            rollup_name,
            body.clone(),
            signature.clone(),
            sequencer_public_key.clone(),
            None,
            utxos.clone(),
            address.clone(),
            546,
            12.0,
            10.0,
            bitcoin::Network::Bitcoin,
            tx_prefix,
        )
        .unwrap();

        // check pow
        assert!(reveal.id.as_byte_array().starts_with(tx_prefix));

        // check outputs
        assert_eq!(commit.output.len(), 2, "commit tx should have 2 outputs");

        let reveal = reveal.tx;
        assert_eq!(reveal.output.len(), 1, "reveal tx should have 1 output");

        assert_eq!(
            commit.input[0].previous_output.txid, utxos[2].tx_id,
            "utxo to inscribe should be chosen correctly"
        );
        assert_eq!(
            commit.input[0].previous_output.vout, utxos[2].vout,
            "utxo to inscribe should be chosen correctly"
        );

        assert_eq!(
            reveal.input[0].previous_output.txid,
            commit.compute_txid(),
            "reveal should use commit as input"
        );
        assert_eq!(
            reveal.input[0].previous_output.vout, 0,
            "reveal should use commit as input"
        );

        assert_eq!(
            reveal.output[0].script_pubkey,
            address.script_pubkey(),
            "reveal should pay to the correct address"
        );

        // check inscription
        let inscription = parse_transaction(&reveal, rollup_name).unwrap();

        assert_eq!(inscription.body, body, "body should be correct");
        assert_eq!(
            inscription.signature, signature,
            "signature should be correct"
        );
        assert_eq!(
            inscription.public_key, sequencer_public_key,
            "sequencer public key should be correct"
        );
    }
}
