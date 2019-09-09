/*
 copyright: (c) 2013-2018 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use chainstate::burn::operations::Error as op_error;
use chainstate::burn::ConsensusHash;
use chainstate::burn::Opcodes;

use chainstate::burn::operations::{
    LeaderBlockCommitOp,
    LeaderKeyRegisterOp,
    UserBurnSupportOp,
    BlockstackOperation,
    BlockstackOperationType,
};

use util::db::DBConn;
use util::db::DBTx;

use chainstate::burn::db::burndb::BurnDB;

use burnchains::BurnchainTransaction;
use burnchains::Txid;
use burnchains::Address;
use burnchains::PublicKey;
use burnchains::BurnchainHeaderHash;
use burnchains::BurnchainBlockHeader;
use burnchains::Burnchain;

use address::AddressHashMode;

use chainstate::stacks::StacksAddress;
use chainstate::stacks::StacksPublicKey;
use chainstate::stacks::StacksPrivateKey;

use util::vrf::{VRF,VRFPublicKey,VRFPrivateKey};

use util::log;
use util::hash::DoubleSha256;

struct ParsedData {
    pub consensus_hash: ConsensusHash,
    pub public_key: VRFPublicKey,
    pub memo: Vec<u8>
}

impl LeaderKeyRegisterOp {
    #[cfg(test)]
    pub fn new(sender: &StacksAddress, public_key: &VRFPublicKey) -> LeaderKeyRegisterOp {
        LeaderKeyRegisterOp {
            public_key: public_key.clone(),
            memo: vec![],
            address: sender.clone(),

            // will be filled in
            consensus_hash: ConsensusHash([0u8; 20]),
            txid: Txid([0u8; 32]),
            vtxindex: 0,
            block_height: 0,
            burn_header_hash: BurnchainHeaderHash([0u8; 32]),
            fork_segment_id: 0
        }
    }

    #[cfg(test)]
    pub fn new_from_secrets(privks: &Vec<StacksPrivateKey>, num_sigs: u16, hash_mode: &AddressHashMode, prover_key: &VRFPrivateKey) -> Option<LeaderKeyRegisterOp> {
        let pubks = privks.iter().map(|ref pk| StacksPublicKey::from_private(pk)).collect();
        let addr = match StacksAddress::from_public_keys(hash_mode.to_version_testnet(), hash_mode, num_sigs as usize, &pubks) {
            Some(a) => {
                a
            },
            None => {
                return None;
            }
        };
        let prover_pubk = VRFPublicKey::from_private(prover_key);
        Some(LeaderKeyRegisterOp::new(&addr, &prover_pubk))
    }
    
    #[cfg(test)]
    pub fn set_mined_at(&mut self, burnchain: &Burnchain, consensus_hash: &ConsensusHash, block_header: &BurnchainBlockHeader) -> () {
        if self.consensus_hash != ConsensusHash([0u8; 20]) {
            self.consensus_hash = consensus_hash.clone();
        }

        if self.txid != Txid([0u8; 32]) {
            self.txid = Txid::from_test_data(block_header.block_height, self.vtxindex, &block_header.block_hash);
        }
        
        if self.burn_header_hash != BurnchainHeaderHash([0u8; 32]) {
            self.burn_header_hash = block_header.block_hash.clone();
        }
    
        self.block_height = block_header.block_height;
        self.fork_segment_id = block_header.fork_segment_id;
    }


    fn parse_data(data: &Vec<u8>) -> Option<ParsedData> {
        /*
            Wire format:

            0      2  3              23                       55                          80
            |------|--|---------------|-----------------------|---------------------------|
             magic  op consensus hash   proving public key               memo

            
             Note that `data` is missing the first 3 bytes -- the magic and op have been stripped
        */
        // memo can be empty, and magic + op are omitted 
        if data.len() < 52 {
            // too short to have a consensus hash and proving public key
            warn!("LEADER_KEY_REGISTER payload is malformed ({} bytes)", data.len());
            return None;
        }

        let consensus_hash = ConsensusHash::from_bytes(&data[0..20]).expect("FATAL: invalid byte slice for consensus hash");
        let pubkey = match VRFPublicKey::from_bytes(&data[20..52].to_vec()) {
            Some(pubk) => {
                pubk
            },
            None => {
                warn!("Invalid VRF public key");
                return None;
            }
        };

        let memo = &data[52..];

        Some(ParsedData {
            consensus_hash,
            public_key: pubkey,
            memo: memo.to_vec()
        })
    }

    fn parse_from_tx(block_height: u64, fork_segment_id: u64, block_hash: &BurnchainHeaderHash, tx: &BurnchainTransaction) -> Result<LeaderKeyRegisterOp, op_error> {
        // can't be too careful...
        let inputs = tx.get_signers();
        let outputs = tx.get_recipients();

        if inputs.len() == 0 {
            test_debug!("Invalid tx: inputs: {}, outputs: {}", inputs.len(), outputs.len());
            return Err(op_error::InvalidInput);
        }

        if outputs.len() < 1 {
            test_debug!("Invalid tx: inputs: {}, outputs: {}", inputs.len(), outputs.len());
            return Err(op_error::InvalidInput);
        }

        if tx.opcode() != Opcodes::LeaderKeyRegister as u8 {
            test_debug!("Invalid tx: invalid opcode {}", tx.opcode());
            return Err(op_error::InvalidInput);
        }

        let data = match LeaderKeyRegisterOp::parse_data(&tx.data()) {
            Some(data) => {
                data
            },
            None => {
                test_debug!("Invalid tx data");
                return Err(op_error::ParseError);
            }
        };

        let address = outputs[0].address.clone();

        Ok(LeaderKeyRegisterOp {
            consensus_hash: data.consensus_hash,
            public_key: data.public_key,
            memo: data.memo,
            address: address,

            txid: tx.txid(),
            vtxindex: tx.vtxindex(),
            block_height: block_height,
            burn_header_hash: block_hash.clone(),

            fork_segment_id: fork_segment_id
        })
    }
}

impl BlockstackOperation for LeaderKeyRegisterOp {
    fn from_tx(block_header: &BurnchainBlockHeader, tx: &BurnchainTransaction) -> Result<LeaderKeyRegisterOp, op_error> {
        LeaderKeyRegisterOp::parse_from_tx(block_header.block_height, block_header.fork_segment_id, &block_header.block_hash, tx)
    }

    fn check<'a>(&self, burnchain: &Burnchain, block_header: &BurnchainBlockHeader, tx: &mut DBTx<'a>) -> Result<(), op_error> {
        // this will be the chain tip we're building on
        let chain_tip = BurnDB::get_block_snapshot(tx, &block_header.parent_block_hash)
            .expect("FATAL: failed to query parent block snapshot")
            .expect("FATAL: no parent snapshot in the DB");

        /////////////////////////////////////////////////////////////////
        // Keys must be unique -- no one can register the same key twice
        /////////////////////////////////////////////////////////////////

        // key selected here must never have been submitted on this fork before 
        let has_key_already = BurnDB::has_VRF_public_key(tx, &self.public_key, chain_tip.fork_segment_id)
            .expect("Sqlite failure while fetching VRF public key");

        if has_key_already {
            warn!("Invalid leader key registration: public key {} previously used", &self.public_key.to_hex());
            return Err(op_error::LeaderKeyAlreadyRegistered);
        }

        /////////////////////////////////////////////////////////////////
        // Consensus hash must be recent and valid
        /////////////////////////////////////////////////////////////////

        let consensus_hash_recent = BurnDB::is_fresh_consensus_hash(tx, chain_tip.block_height, burnchain.consensus_hash_lifetime.into(), &self.consensus_hash, chain_tip.fork_segment_id)
            .expect("Sqlite failure while checking consensus hash freshness");

        if !consensus_hash_recent {
            warn!("Invalid leader key registration: invalid consensus hash {}", &self.consensus_hash.to_hex());
            return Err(op_error::LeaderKeyBadConsensusHash);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burnchains::bitcoin::address::BitcoinAddress;
    use burnchains::bitcoin::keys::BitcoinPublicKey;
    use burnchains::bitcoin::blocks::BitcoinBlockParser;
    use burnchains::bitcoin::BitcoinNetworkType;
    use burnchains::Txid;
    use burnchains::BurnchainBlockHeader;
    use burnchains::BLOCKSTACK_MAGIC_MAINNET;

    use deps::bitcoin::network::serialize::deserialize;
    use deps::bitcoin::blockdata::transaction::Transaction;

    use chainstate::burn::{ConsensusHash, OpsHash, SortitionHash, BlockSnapshot};
    
    use util::hash::hex_bytes;
    use util::log;
    
    use chainstate::burn::operations::{
        LeaderBlockCommitOp,
        LeaderKeyRegisterOp,
        UserBurnSupportOp,
        BlockstackOperation,
        BlockstackOperationType
    };

    struct OpFixture {
        txstr: String,
        result: Option<LeaderKeyRegisterOp>,
    }

    struct CheckFixture {
        op: LeaderKeyRegisterOp,
        res: Result<(), op_error>
    }

    fn make_tx(hex_str: &str) -> Result<Transaction, &'static str> {
        let tx_bin = hex_bytes(hex_str)
            .map_err(|_e| "failed to decode hex string")?;
        let tx = deserialize(&tx_bin.to_vec())
            .map_err(|_e| "failed to deserialize")?;
        Ok(tx)
    }

    #[test]
    fn test_parse() {
        let vtxindex = 1;
        let block_height = 694;
        let burn_header_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let tx_fixtures: Vec<OpFixture> = vec![
            OpFixture {
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006a47304402203a176d95803e8d51e7884d38750322c4bfa55307a71291ef8db65191edd665f1022056f5d1720d1fde8d6a163c79f73f22f874ef9e186e98e5b60fa8ac64d298e77a012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0000000000200000000000000003e6a3c69645e2222222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a010203040539300000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: Some(LeaderKeyRegisterOp {
                    consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
                    public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
                    memo: vec![01, 02, 03, 04, 05],
                    address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

                    txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
                    vtxindex: vtxindex,
                    block_height: block_height,
                    burn_header_hash: burn_header_hash.clone(),

                    fork_segment_id: 0,
                })
            },
            OpFixture {
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006a473044022037d0b9d4e98eab190522acf5fb8ea8e89b6a4704e0ac6c1883d6ffa629b3edd30220202757d710ec0fb940d1715e02588bb2150110161a9ee08a83b750d961431a8e012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000020000000000000000396a3769645e2222222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a39300000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: Some(LeaderKeyRegisterOp {
                    consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
                    public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
                    memo: vec![],
                    address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

                    txid: Txid::from_bytes_be(&hex_bytes("2fbf8d5be32dce49790d203ba59acbb0929d5243413174ff5d26a5c6f23dea65").unwrap()).unwrap(),
                    vtxindex: vtxindex,
                    block_height: block_height,
                    burn_header_hash: burn_header_hash,
                    
                    fork_segment_id: 0,
                })
            },
            OpFixture {
                // invalid VRF public key 
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100ddbbaf029174a9bd1588fc0b34094e9f48fec9c89704eb12a3ee70dd5ca4142e02202eab7cbf985da23e890766331f7e0009268d1db75da8b583a953528e6a099499012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0000000000200000000000000003e6a3c69645e2222222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7b010203040539300000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: None,
            },
            OpFixture {
                // too short
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100b2680431ab771826f42b93f5238e518c6483af7026c25ddd6e970f26fec80473022050ab510ede8d7b50cea1a286d1e05fa2b2d62ffbb9983e4cade9899474d0f8b9012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000020000000000000000386a3669645e22222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a39300000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: None,
            },
            OpFixture {
                // not enough outputs
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006a473044022070c8ce3786cee46d283b8a02a9c6ba87ef693960a0200b4a85e1b4808ea7b23a02201c6926162fe8cf4d3bbc3fcea80baa8307543af69b5dbbad72aa659a3a87f08e012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0000000000100000000000000003e6a3c69645e2222222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a010203040500000000".to_string(),
                result: None,
            },
            OpFixture {
                // wrong opcode
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100a72df03441bdd08b8fd042f417e37e7ba7dc6212078835840f4cbd64f690533a0220385309a6096044828ec7889107a73da23b009157a752251ed68f8084834d4d44012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0000000000200000000000000003e6a3c69645f2222222222222222222222222222222222222222a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a010203040539300000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: None,
            }
        ];

        let parser = BitcoinBlockParser::new(BitcoinNetworkType::Testnet, BLOCKSTACK_MAGIC_MAINNET);

        for tx_fixture in tx_fixtures {
            let tx = make_tx(&tx_fixture.txstr).unwrap();
            let header = match tx_fixture.result {
                Some(ref op) => {
                    BurnchainBlockHeader {
                        block_height: op.block_height,
                        block_hash: op.burn_header_hash.clone(),
                        parent_block_hash: op.burn_header_hash.clone(),
                        num_txs: 1,
                        fork_segment_id: op.fork_segment_id,
                        parent_fork_segment_id: op.fork_segment_id,
                        fork_segment_length: 1,
                        fork_length: 1,
                    }
                },
                None => {
                    BurnchainBlockHeader {
                        block_height: 0,
                        block_hash: BurnchainHeaderHash([0u8; 32]),
                        parent_block_hash: BurnchainHeaderHash([0u8; 32]),
                        num_txs: 0,
                        fork_segment_id: 0,
                        parent_fork_segment_id: 0,
                        fork_segment_length: 0,
                        fork_length: 0,
                    }
                }
            };
            let burnchain_tx = BurnchainTransaction::Bitcoin(parser.parse_tx(&tx, vtxindex as usize).unwrap());
            let op = LeaderKeyRegisterOp::from_tx(&header, &burnchain_tx);

            match (op, tx_fixture.result) {
                (Ok(parsed_tx), Some(result)) => {
                    assert_eq!(parsed_tx, result);
                },
                (Err(_e), None) => {},
                (Ok(_parsed_tx), None) => {
                    test_debug!("Parsed a tx when we should not have: {}", tx_fixture.txstr);
                    assert!(false);
                },
                (Err(_e), Some(_result)) => {
                    test_debug!("Did not parse a tx when we should have: {}", tx_fixture.txstr);
                    assert!(false);
                }
            };
        }
    }

    #[test]
    fn test_check() {
        
        let first_block_height = 120;
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000123").unwrap();
        
        let block_122_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000002").unwrap();
        let block_123_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000003").unwrap();
        let block_124_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000004").unwrap();
        let block_125_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000005").unwrap();
        
        let burnchain = Burnchain {
            peer_version: 0x012345678,
            network_id: 0x9abcdef0,
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".to_string(),
            working_dir: "/nope".to_string(),
            consensus_hash_lifetime: 24,
            stable_confirmations: 7,
            first_block_height: first_block_height,
            first_block_hash: first_burn_hash.clone()
        };
        
        let mut db = BurnDB::connect_memory(first_block_height, &first_burn_hash).unwrap();

        let leader_key_1 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

            txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
            vtxindex: 456,
            block_height: 123,
            burn_header_hash: block_123_hash.clone(),
            
            fork_segment_id: 0,
        };
        
        // populate consensus hashes
        {
            let mut tx = db.tx_begin().unwrap();
            let mut prev_snapshot = BurnDB::get_first_block_snapshot(&mut tx).unwrap();
            for i in 0..10 {
                let snapshot_row = BlockSnapshot {
                    block_height: i + 1 + first_block_height,
                    burn_header_hash: BurnchainHeaderHash::from_bytes(&[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i as u8]).unwrap(),
                    parent_burn_header_hash: prev_snapshot.burn_header_hash.clone(),
                    consensus_hash: ConsensusHash::from_bytes(&[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i as u8]).unwrap(),
                    ops_hash: OpsHash::from_bytes(&[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i as u8]).unwrap(),
                    total_burn: i,
                    sortition: true,
                    sortition_hash: SortitionHash::initial(),
                    winning_block_txid: Txid::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                    winning_block_burn_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),

                    fork_segment_id: 0,
                    parent_fork_segment_id: 0,
                    fork_segment_length: i + 1,
                    fork_length: i + 1
                };
                BurnDB::append_chain_tip_snapshot(&mut tx, &prev_snapshot, &snapshot_row).unwrap();
                prev_snapshot = snapshot_row;
            }
            
            tx.commit().unwrap();
        }

        {
            let mut tx = db.tx_begin().unwrap();
            BurnDB::insert_leader_key(&mut tx, &leader_key_1).unwrap();
            tx.commit().unwrap();
        }

        let check_fixtures = vec![
            CheckFixture {
                // reject -- key already registered 
                op: LeaderKeyRegisterOp {
                    consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
                    public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
                    memo: vec![01, 02, 03, 04, 05],
                    address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

                    txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
                    vtxindex: 455,
                    block_height: 122,
                    burn_header_hash: block_123_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::LeaderKeyAlreadyRegistered),
            },
            CheckFixture {
                // reject -- invalid consensus hash
                op: LeaderKeyRegisterOp {
                    consensus_hash: ConsensusHash::from_bytes(&hex_bytes("1000000000000000000000000000000000000000").unwrap()).unwrap(),
                    public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
                    memo: vec![01, 02, 03, 04, 05],
                    address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

                    txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
                    vtxindex: 456,
                    block_height: 123,
                    burn_header_hash: block_123_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::LeaderKeyBadConsensusHash),
            },
            CheckFixture {
                // accept 
                op: LeaderKeyRegisterOp {
                    consensus_hash: ConsensusHash::from_bytes(&hex_bytes("0000000000000000000000000000000000000000").unwrap()).unwrap(),
                    public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
                    memo: vec![01, 02, 03, 04, 05],
                    address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a9140be3e286a15ea85882761618e366586b5574100d88ac").unwrap()).unwrap()),

                    txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
                    vtxindex: 456,
                    block_height: 123,
                    burn_header_hash: block_123_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Ok(())
            }
        ];

        for fixture in check_fixtures {
            let mut tx = db.tx_begin().unwrap();
            let header = BurnchainBlockHeader {
                block_height: fixture.op.block_height,
                block_hash: fixture.op.burn_header_hash.clone(),
                parent_block_hash: fixture.op.burn_header_hash.clone(),
                num_txs: 1,
                fork_segment_id: fixture.op.fork_segment_id,
                parent_fork_segment_id: fixture.op.fork_segment_id,
                fork_segment_length: 1,
                fork_length: 1,
            };
            assert_eq!(fixture.res, fixture.op.check(&burnchain, &header, &mut tx));
        }
    }
}

