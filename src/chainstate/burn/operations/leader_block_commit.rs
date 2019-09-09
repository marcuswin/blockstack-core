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

use address::AddressHashMode;
use chainstate::burn::ConsensusHash;
use chainstate::burn::operations::Error as op_error;
use chainstate::burn::Opcodes;
use chainstate::burn::{BlockHeaderHash, VRFSeed};

use chainstate::burn::db::burndb::BurnDB;

use chainstate::burn::operations::{
    LeaderBlockCommitOp,
    LeaderKeyRegisterOp,
    UserBurnSupportOp,
    BlockstackOperation,
    BlockstackOperationType,
    parse_u32_from_be,
    parse_u16_from_be
};

use chainstate::stacks::StacksPublicKey;
use chainstate::stacks::StacksPrivateKey;

use burnchains::{BurnchainTransaction, PublicKey};
use burnchains::Txid;
use burnchains::Address;
use burnchains::BurnchainHeaderHash;
use burnchains::Burnchain;
use burnchains::BurnchainBlockHeader;
use burnchains::{
    BurnchainSigner,
    BurnchainRecipient
};

use util::log;
use util::hash::to_hex;
use util::vrf::VRF;
use util::vrf::VRFPublicKey;
use util::vrf::VRFPrivateKey;
use util::db::DBConn;
use util::db::DBTx;

// return type from parse_data below
struct ParsedData {
    block_header_hash: BlockHeaderHash,
    new_seed: VRFSeed,
    parent_block_backptr: u16,
    parent_vtxindex: u16,
    key_block_backptr: u16,
    key_vtxindex: u16,
    epoch_num: u32,
    memo: Vec<u8>
}

impl LeaderBlockCommitOp {
    #[cfg(test)]
    pub fn initial(block_header_hash: &BlockHeaderHash, paired_key: &LeaderKeyRegisterOp, burn_fee: u64, input: &BurnchainSigner) -> LeaderBlockCommitOp {
        LeaderBlockCommitOp {
            new_seed: VRFSeed([0u8; 32]),
            key_vtxindex: paired_key.vtxindex as u16,
            parent_vtxindex: 0,
            memo: vec![0x00],       // informs mined_at not to touch parent backkptrs
            burn_fee: burn_fee,
            input: input.clone(),
            block_header_hash: block_header_hash.clone(),

            // partially filled in
            epoch_num: 0,
            parent_block_backptr: 0,
            key_block_backptr: paired_key.block_height as u16,

            // to be filled in 
            txid: Txid([0u8; 32]),
            vtxindex: 0,
            block_height: 0,
            burn_header_hash: BurnchainHeaderHash([0u8; 32]),

            fork_segment_id: 0
        }
    }

    #[cfg(test)]
    pub fn new(block_header_hash: &BlockHeaderHash, new_seed: &VRFSeed, parent: &LeaderBlockCommitOp, key_block_height: u16, key_vtxindex: u16, burn_fee: u64, input: &BurnchainSigner) -> LeaderBlockCommitOp {
        LeaderBlockCommitOp {
            new_seed: new_seed.clone(),
            key_vtxindex: key_vtxindex,
            parent_vtxindex: parent.vtxindex as u16,
            memo: vec![],
            burn_fee: burn_fee,
            input: input.clone(),
            block_header_hash: block_header_hash.clone(),

            // partially filled in
            epoch_num: 0,
            parent_block_backptr: parent.block_height as u16,
            key_block_backptr: key_block_height as u16,

            // to be filled in
            txid: Txid([0u8; 32]),
            vtxindex: 0,
            block_height: 0,
            burn_header_hash: BurnchainHeaderHash([0u8; 32]),

            fork_segment_id: 0,
        }
    }

    #[cfg(test)]
    pub fn new_from_secrets<'a>(tx: &mut DBTx<'a>, privks: &Vec<StacksPrivateKey>, num_sigs: u16, hash_mode: &AddressHashMode, prover_key: &VRFPrivateKey, key_block_height: u16, key_vtxindex: u16, block_hash: &BlockHeaderHash, burn_fee: u64) -> Option<LeaderBlockCommitOp> {
        let pubks = privks.iter().map(|ref pk| StacksPublicKey::from_private(pk)).collect();
        let input = BurnchainSigner {
            hash_mode: hash_mode.clone(),
            num_sigs: num_sigs as usize,
            public_keys: pubks
        };

        let chain_tip = BurnDB::get_canonical_chain_tip(tx).expect("FATAL: failed to read canonical chain tip");
        let last_block_height = chain_tip.block_height;
        let fork_segment_id = chain_tip.fork_segment_id;
        let last_snapshot = BurnDB::get_last_snapshot_with_sortition(tx, last_block_height, fork_segment_id).expect("FATAL: failed to read last snapshot");
        let parent = match BurnDB::get_block_commit(tx, &last_snapshot.winning_block_txid, &last_snapshot.winning_block_burn_hash, last_snapshot.fork_segment_id).expect("FATAL: failed to read block commit") {
            Some(p) => {
                p
            },
            None => {
                return None;
            }
        };

        let prover_pubk = VRFPublicKey::from_private(prover_key);

        // prove on the parent's seed to produce the new seed
        let proof = VRF::prove(prover_key, &parent.new_seed.as_bytes().to_vec());
        let new_seed = VRFSeed::from_proof(&proof);
        
        Some(LeaderBlockCommitOp::new(block_hash, &new_seed, &parent, key_block_height, key_vtxindex, burn_fee, &input))
    }

    #[cfg(test)]
    pub fn set_mined_at(&mut self, burnchain: &Burnchain, consensus_hash: &ConsensusHash, block_header: &BurnchainBlockHeader) -> () {
        self.key_block_backptr = (block_header.block_height - (self.key_block_backptr as u64)) as u16;

        if self.memo.len() == 0 {
            self.parent_block_backptr = (block_header.block_height - (self.parent_block_backptr as u64)) as u16;
        }

        self.epoch_num = (block_header.block_height - burnchain.first_block_height) as u32;

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
            TODO: pick one of these.

            TODO: we probably don't need to commit to the PoW difficulty on-chain if all we're doing is training miners.
            we can add it as something committed to in the MARF, so we can probably soft-fork it in if needed
            (assuming we want to make the transition to native PoW at all).

            Hybrid PoB/PoW Wire format:
            0      2  3               34               67     68     70    71   72     76    80
            |------|--|----------------|---------------|------|------|-----|-----|-----|-----|
             magic  op   block hash       new seed     parent parent key   key   epoch  PoW
                       (31-byte; lead 0)               delta  txoff  delta txoff num.   nonce

             Note that `data` is missing the first 3 bytes -- the magic and op have been stripped

             The values parent-txoff and key-txoff are in network byte order.

            Wire format:
            0      2  3            35               67     69     71    73   75     79    80
            |------|--|-------------|---------------|------|------|-----|-----|-----|-----|
             magic  op   block hash     new seed     parent parent key   key   epoch  memo
                                                     delta  txoff  delta txoff num.

             Note that `data` is missing the first 3 bytes -- the magic and op have been stripped

             The values parent-delta, parent-txoff, key-delta, and key-txoff are in network byte order.

             parent-delta and parent-txoff will both be 0 if this block builds off of the genesis block.
        */

        if data.len() < 77 {
            // too short
            warn!("LEADER_BLOCK_COMMIT payload is malformed ({} bytes)", data.len());
            return None;
        }

        let block_header_hash = BlockHeaderHash::from_bytes(&data[0..32]).unwrap();
        let new_seed = VRFSeed::from_bytes(&data[32..64]).unwrap();
        let parent_block_backptr = parse_u16_from_be(&data[64..66]).unwrap();
        let parent_vtxindex = parse_u16_from_be(&data[66..68]).unwrap();
        let key_block_backptr = parse_u16_from_be(&data[68..70]).unwrap();
        let key_vtxindex = parse_u16_from_be(&data[70..72]).unwrap();
        let epoch_num = parse_u32_from_be(&data[72..76]).unwrap();
        let memo = data[76..77].to_vec();

        Some(ParsedData {
            block_header_hash,
            new_seed,
            parent_block_backptr,
            parent_vtxindex,
            key_block_backptr,
            key_vtxindex,
            epoch_num,
            memo
        })
    }

    fn parse_from_tx(block_height: u64, fork_segment_id: u64, block_hash: &BurnchainHeaderHash, tx: &BurnchainTransaction) -> Result<LeaderBlockCommitOp, op_error> {
        // can't be too careful...
        let inputs = tx.get_signers();
        let outputs = tx.get_recipients();

        if inputs.len() == 0 {
            warn!("Invalid tx: inputs: {}, outputs: {}", inputs.len(), outputs.len());
            return Err(op_error::InvalidInput);
        }

        if outputs.len() == 0 {
            warn!("Invalid tx: inputs: {}, outputs: {}", inputs.len(), outputs.len());
            return Err(op_error::InvalidInput);
        }

        if tx.opcode() != (Opcodes::LeaderBlockCommit as u8) {
            warn!("Invalid tx: invalid opcode {}", tx.opcode());
            return Err(op_error::InvalidInput);
        }

        // outputs[0] should be the burn output
        if !outputs[0].address.is_burn() {
            // wrong burn output
            warn!("Invalid tx: burn output missing (got {:?})", outputs[0]);
            return Err(op_error::ParseError);
        }

        let burn_fee = outputs[0].amount;
        if burn_fee == 0 {
            // didn't burn
            warn!("Invalid tx: no burn quantity");
            return Err(op_error::ParseError);
        }

        let data = match LeaderBlockCommitOp::parse_data(&tx.data()) {
            None => {
                warn!("Invalid tx data");
                return Err(op_error::ParseError);
            },
            Some(d) => d
        };

        // basic sanity checks 
        if data.parent_block_backptr == 0 {
            if data.parent_vtxindex != 0 {
                warn!("Invalid tx: parent block back-pointer must be positive");
                return Err(op_error::ParseError);
            }
            // if parent block backptr and parent vtxindex are both 0, then this block's parent is
            // the genesis block.
        }

        if data.parent_block_backptr as u64 >= block_height {
            warn!("Invalid tx: parent block back-pointer {} exceeds block height {}", data.parent_block_backptr, block_height);
            return Err(op_error::ParseError);
        }

        if data.key_block_backptr == 0 {
            warn!("Invalid tx: key block back-pointer must be positive");
            return Err(op_error::ParseError);
        }

        if data.key_block_backptr as u64 >= block_height {
            warn!("Invalid tx: key block back-pointer {} exceeds block height {}", data.key_block_backptr, block_height);
            return Err(op_error::ParseError);
        }

        if data.epoch_num as u64 >= block_height {
            warn!("Invalid tx: epoch number {} exceeds block height {}", data.epoch_num, block_height);
            return Err(op_error::ParseError);
        }

        Ok(LeaderBlockCommitOp {
            block_header_hash: data.block_header_hash,
            new_seed: data.new_seed,
            parent_block_backptr: data.parent_block_backptr,
            parent_vtxindex: data.parent_vtxindex,
            key_block_backptr: data.key_block_backptr,
            key_vtxindex: data.key_vtxindex,
            epoch_num: data.epoch_num,
            memo: data.memo,

            burn_fee: burn_fee,
            input: inputs[0].clone(),

            txid: tx.txid(),
            vtxindex: tx.vtxindex(),
            block_height: block_height,
            burn_header_hash: block_hash.clone(),

            fork_segment_id: fork_segment_id
        })
    }
}

impl BlockstackOperation for LeaderBlockCommitOp {
    fn from_tx(block_header: &BurnchainBlockHeader, tx: &BurnchainTransaction) -> Result<LeaderBlockCommitOp, op_error> {
        LeaderBlockCommitOp::parse_from_tx(block_header.block_height, block_header.fork_segment_id, &block_header.block_hash, tx)
    }
        
    fn check<'a>(&self, burnchain: &Burnchain, block_header: &BurnchainBlockHeader, tx: &mut DBTx<'a>) -> Result<(), op_error> {
        let leader_key_block_height = self.block_height - (self.key_block_backptr as u64);
        let parent_block_height = self.block_height - (self.parent_block_backptr as u64);

        /////////////////////////////////////////////////////////////////////////////////////
        // There must be a burn
        /////////////////////////////////////////////////////////////////////////////////////
        if self.burn_fee == 0 {
            warn!("Invalid block commit: no burn amount");
            return Err(op_error::BlockCommitBadInput);
        }
        
        /////////////////////////////////////////////////////////////////////////////////////
        // This tx's epoch number must match the current epoch
        /////////////////////////////////////////////////////////////////////////////////////
    
        let first_block_snapshot = BurnDB::get_first_block_snapshot(tx)
            .expect("FATAL: failed to query first block snapshot");

        if self.block_height < first_block_snapshot.block_height {
            warn!("Invalid block commit: predates genesis height {}", first_block_snapshot.block_height);
            return Err(op_error::BlockCommitPredatesGenesis);
        }

        let target_epoch = self.block_height - first_block_snapshot.block_height;
        if (self.epoch_num as u64) != target_epoch {
            warn!("Invalid block commit: current epoch is {}; got {}", target_epoch, self.epoch_num);
            return Err(op_error::BlockCommitBadEpoch);
        }
        
        /////////////////////////////////////////////////////////////////////////////////////
        // There must exist a previously-accepted *unused* key from a LeaderKeyRegister
        /////////////////////////////////////////////////////////////////////////////////////

        if self.key_block_backptr == 0 {
            warn!("Invalid block commit: references leader key in the same block");
            return Err(op_error::BlockCommitNoLeaderKey);
        }

        // this will be the chain tip we're building on
        let chain_tip = BurnDB::get_block_snapshot(tx, &block_header.parent_block_hash)
            .expect("FATAL: failed to query parent block snapshot")
            .expect("FATAL: no parent snapshot in the DB");

        let register_key_opt = BurnDB::get_leader_key_at(tx, leader_key_block_height, self.key_vtxindex.into(), chain_tip.fork_segment_id)
            .expect("Sqlite failure while getting a prior leader VRF key");

        if register_key_opt.is_none() {
            warn!("Invalid block commit: no corresponding leader key at {},{} in fork {}", leader_key_block_height, self.key_vtxindex, chain_tip.fork_segment_id);
            return Err(op_error::BlockCommitNoLeaderKey);
        }

        let register_key = register_key_opt.unwrap();
    
        let is_key_consumed = BurnDB::is_leader_key_consumed(tx, chain_tip.block_height, &register_key, chain_tip.fork_segment_id)
            .expect("Sqlite failure while verifying that a leader VRF key is not consumed");

        if is_key_consumed {
            warn!("Invalid block commit: leader key at ({},{}) is already used as of {} in fork {}", register_key.block_height, register_key.vtxindex, chain_tip.block_height, chain_tip.fork_segment_id);
            return Err(op_error::BlockCommitLeaderKeyAlreadyUsed);
        }

        /////////////////////////////////////////////////////////////////////////////////////
        // There must exist a previously-accepted block from a LeaderBlockCommit, or this
        // LeaderBlockCommit must build off of the genesis block.  If _not_ building off of the
        // genesis block, then the parent block must be in a different epoch (i.e. its parent must
        // be committed already).
        /////////////////////////////////////////////////////////////////////////////////////

        if self.parent_block_backptr == 0 && self.parent_vtxindex != 0 {
            // tried to build off a block in the same epoch (not allowed)
            warn!("Invalid block commit: cannot build off of a commit in the same epoch");
            return Err(op_error::BlockCommitNoParent);
        }
        else if self.parent_block_backptr != 0 || self.parent_vtxindex != 0 {
            // not building off of genesis
            let parent_block_opt = BurnDB::get_block_commit_at(tx, parent_block_height, self.parent_vtxindex.into(), chain_tip.fork_segment_id)
                .expect("Sqlite failure while verifying that this block commitment is new");

            if parent_block_opt.is_none() {
                warn!("Invalid block commit: no corresponding parent block");
                return Err(op_error::BlockCommitNoParent);
            }
        }
        
        /////////////////////////////////////////////////////////////////////////////////////
        // This LeaderBlockCommit's input public keys must match the address of the LeaderKeyRegister
        // -- the hash of the inputs' public key(s) must equal the hash contained within the
        // LeaderKeyRegister's address.  Note that we only need to check the address bytes,
        // not the entire address (since finding two sets of different public keys that
        // hash to the same address is considered intractible).
        //
        // Under the hood, the blockchain further ensures that the tx was signed with the
        // associated private keys, so only the private key owner(s) are in a position to 
        // reveal the keys that hash to the address's hash.
        /////////////////////////////////////////////////////////////////////////////////////

        let input_address_bytes = self.input.to_address_bits();
        let addr_bytes = register_key.address.to_bytes();

        if input_address_bytes != addr_bytes {
            warn!("Invalid block commit: leader key at ({},{}) has address bytes {}, but this tx input has address bytes {}",
                  register_key.block_height, register_key.vtxindex, &to_hex(&addr_bytes), &to_hex(&input_address_bytes[..]));
            return Err(op_error::BlockCommitBadInput);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use burnchains::bitcoin::keys::BitcoinPublicKey;
    use burnchains::bitcoin::address::BitcoinAddress;
    use burnchains::bitcoin::blocks::BitcoinBlockParser;
    use burnchains::Txid;
    use burnchains::BLOCKSTACK_MAGIC_MAINNET;
    use burnchains::BurnchainBlockHeader;

    use burnchains::bitcoin::BitcoinNetworkType;

    use address::AddressHashMode;

    use deps::bitcoin::network::serialize::deserialize;
    use deps::bitcoin::blockdata::transaction::Transaction;
    
    use chainstate::burn::{BlockHeaderHash, ConsensusHash, VRFSeed};
    
    use chainstate::burn::operations::{
        LeaderBlockCommitOp,
        LeaderKeyRegisterOp,
        UserBurnSupportOp,
        BlockstackOperation,
        BlockstackOperationType
    };

    use util::vrf::VRFPublicKey;
    use util::hash::hex_bytes;
    use util::log;
    
    use chainstate::stacks::StacksAddress;
    use chainstate::stacks::StacksPublicKey;

    use chainstate::burn::OpsHash;
    use chainstate::burn::SortitionHash;
    use chainstate::burn::BlockSnapshot;

    struct OpFixture {
        txstr: String,
        result: Option<LeaderBlockCommitOp>
    }

    struct CheckFixture {
        op: LeaderBlockCommitOp,
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
        let block_height = 0x71706363;  // epoch number must be strictly smaller than block height
        let burn_header_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap();

        let tx_fixtures = vec![
            OpFixture {
                // valid
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100eba8c0a57c1eb71cdfba0874de63cf37b3aace1e56dcbd61701548194a79af34022041dd191256f3f8a45562e5d60956bb871421ba69db605716250554b23b08277b012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000030000000000000000536a4c5069645b222222222222222222222222222222222222222222222222222222222222222233333333333333333333333333333333333333333333333333333333333333334041424350516061626370718039300000000000001976a914000000000000000000000000000000000000000088aca05b0000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: Some(LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 0x4140,
                    parent_vtxindex: 0x4342,
                    key_block_backptr: 0x5150,
                    key_vtxindex: 0x6160,
                    epoch_num: 0x71706362,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1, 
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: vtxindex,
                    block_height: block_height,
                    burn_header_hash: burn_header_hash,
                    fork_segment_id: 0,
                })
            },
            OpFixture {
                // invalid -- wrong opcode 
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006946304302207129fa2054a61cdb4b7db0b8fab6e8ff4af0edf979627aa5cf41665b7475a451021f70032b48837df091223c1d0bb57fb0298818eb11d0c966acff4b82f4b2d5c8012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000030000000000000000536a4c5069645c222222222222222222222222222222222222222222222222222222222222222233333333333333333333333333333333333333333333333333333333333333334041424350516061626370718039300000000000001976a914000000000000000000000000000000000000000088aca05b0000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: None,
            },
            OpFixture {
                // invalid -- wrong burn address
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100e25f5f9f660339cd665caba231d5bdfc3f0885bcc0b3f85dc35564058c9089d702206aa142ea6ccd89e56fdc0743cdcf3a2744e133f335e255e9370e4f8a6d0f6ffd012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000030000000000000000536a4c5069645b222222222222222222222222222222222222222222222222222222222222222233333333333333333333333333333333333333333333333333333333333333334041424350516061626370718039300000000000001976a914000000000000000000000000000000000000000188aca05b0000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
                result: None,
            },
            OpFixture {
                // invalid -- bad OP_RETURN (missing memo)
                txstr: "01000000011111111111111111111111111111111111111111111111111111111111111111000000006b483045022100c6c3ccc9b5a6ba5161706f3a5e4518bc3964e8de1cf31dbfa4d38082535c88e902205860de620cfe68a72d5a1fc3be1171e6fd8b2cdde0170f76724faca0db5ee0b6012102d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d000000000030000000000000000526a4c4f69645b2222222222222222222222222222222222222222222222222222222222222222333333333333333333333333333333333333333333333333333333333333333340414243505160616263707139300000000000001976a914000000000000000000000000000000000000000088aca05b0000000000001976a9140be3e286a15ea85882761618e366586b5574100d88ac00000000".to_string(),
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
            let op = LeaderBlockCommitOp::from_tx(&header, &burnchain_tx);

            match (op, tx_fixture.result) {
                (Ok(parsed_tx), Some(result)) => {
                    assert_eq!(parsed_tx, result);
                },
                (Err(_e), None) => {},
                (Ok(_parsed_tx), None) => {
                    test_debug!("Parsed a tx when we should not have");
                    assert!(false);
                },
                (Err(_e), Some(_result)) => {
                    test_debug!("Did not parse a tx when we should have");
                    assert!(false);
                }
            };
        }
    }

    #[test]
    fn test_check() {
        let first_block_height = 121;
        let first_burn_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000123").unwrap();
        
        let block_122_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000001220").unwrap();
        let block_123_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000001230").unwrap();
        let block_124_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000001240").unwrap();
        let block_125_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000001250").unwrap();
        let block_126_hash = BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000001260").unwrap();

        let block_header_hashes = [
            block_122_hash.clone(),
            block_123_hash.clone(),
            block_124_hash.clone(),
            block_125_hash.clone(),
            block_126_hash.clone()
        ];
        
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

        {
            let mut tx = db.tx_begin().unwrap();
            let mut prev_snapshot = BurnDB::get_first_block_snapshot(&mut tx).unwrap();
            for i in 0..block_header_hashes.len() {
                let snapshot_row = BlockSnapshot {
                    block_height: (i + 1 + first_block_height as usize) as u64,
                    burn_header_hash: block_header_hashes[i].clone(),
                    parent_burn_header_hash: prev_snapshot.burn_header_hash.clone(),
                    consensus_hash: ConsensusHash::from_bytes(&[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i as u8]).unwrap(),
                    ops_hash: OpsHash::from_bytes(&[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,i as u8]).unwrap(),
                    total_burn: i as u64,
                    sortition: true,
                    sortition_hash: SortitionHash::initial(),
                    winning_block_txid: Txid::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                    winning_block_burn_hash: BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap(),

                    fork_segment_id: 0,
                    parent_fork_segment_id: 0,
                    fork_segment_length: (i + 1) as u64,
                    fork_length: (i + 1) as u64
                };
                BurnDB::append_chain_tip_snapshot(&mut tx, &prev_snapshot, &snapshot_row).unwrap();
                prev_snapshot = snapshot_row;
            }
            
            tx.commit().unwrap();
        }

        let leader_key_1 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("a366b51292bef4edd64063d9145c617fec373bceb0758e98cd72becd84d54c7a").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a914306231b2782b5f80d944bf69f9d46a1453a0a0eb88ac").unwrap()).unwrap()),

            txid: Txid::from_bytes_be(&hex_bytes("1bfa831b5fc56c858198acb8e77e5863c1e9d8ac26d49ddb914e24d8d4083562").unwrap()).unwrap(),
            vtxindex: 456,
            block_height: 124,
            burn_header_hash: block_124_hash.clone(),
            fork_segment_id: 0,
        };
        
        let leader_key_2 = LeaderKeyRegisterOp { 
            consensus_hash: ConsensusHash::from_bytes(&hex_bytes("3333333333333333333333333333333333333333").unwrap()).unwrap(),
            public_key: VRFPublicKey::from_bytes(&hex_bytes("bb519494643f79f1dea0350e6fb9a1da88dfdb6137117fc2523824a8aa44fe1c").unwrap()).unwrap(),
            memo: vec![01, 02, 03, 04, 05],
            address: StacksAddress::from_bitcoin_address(&BitcoinAddress::from_scriptpubkey(BitcoinNetworkType::Testnet, &hex_bytes("76a914306231b2782b5f80d944bf69f9d46a1453a0a0eb88ac").unwrap()).unwrap()),

            txid: Txid::from_bytes_be(&hex_bytes("9410df84e2b440055c33acb075a0687752df63fe8fe84aeec61abe469f0448c7").unwrap()).unwrap(),
            vtxindex: 457,
            block_height: 124,
            burn_header_hash: block_124_hash.clone(),
            fork_segment_id: 0,
        };

        // consumes leader_key_1
        let block_commit_1 = LeaderBlockCommitOp {
            block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
            new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
            parent_block_backptr: 1,
            parent_vtxindex: 1,
            key_block_backptr: 1,
            key_vtxindex: 456,
            epoch_num: 50,
            memo: vec![0x80],

            burn_fee: 12345,
            input: BurnchainSigner {
                public_keys: vec![
                    StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                ],
                num_sigs: 1, 
                hash_mode: AddressHashMode::SerializeP2PKH
            },

            txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
            vtxindex: 444,
            block_height: 125,
            burn_header_hash: block_125_hash.clone(),
            fork_segment_id: 0,
        };

        {
            let mut tx = db.tx_begin().unwrap();
            BurnDB::insert_leader_key(&mut tx, &leader_key_1).unwrap();
            BurnDB::insert_leader_key(&mut tx, &leader_key_2).unwrap();
            BurnDB::insert_block_commit(&mut tx, &block_commit_1).unwrap();
            tx.commit().unwrap();
        }
        
        let block_height = 124;

        let fixtures = vec![
            CheckFixture {
                // reject -- predates start block
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 50,
                    parent_vtxindex: 456,
                    key_block_backptr: 1,
                    key_vtxindex: 457,
                    epoch_num: 50,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1, 
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 444,
                    block_height: 80,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitPredatesGenesis),
            },
            CheckFixture {
                // reject -- epoch does not match block height 
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: 50,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1, 
                        hash_mode: AddressHashMode::SerializeP2PKH,
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 444,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitBadEpoch),
            },
            CheckFixture {
                // reject -- no such leader key 
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 400,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 444,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitNoLeaderKey),
            },
            CheckFixture {
                // reject -- leader key consumed already
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 456,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitLeaderKeyAlreadyUsed),
            },
            CheckFixture {
                // reject -- previous block must exist 
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 445,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitNoParent),
            },
            CheckFixture {
                // reject -- previous block must exist in a different block 
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 0,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitNoParent),
            },
            CheckFixture {
                // reject -- tx input does not match any leader keys
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("03984286096373539ae529bd997c92792d4e5b5967be72979a42f587a625394116").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitBadInput),
            },
            CheckFixture {
                // reject -- fee is 0 
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 0,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Err(op_error::BlockCommitBadInput)
            },
            CheckFixture {
                // accept -- consumes leader_key_2
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Ok(())
            },
            CheckFixture {
                // accept -- consumes leader_key_2 and starts a new fork segment
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 1,
                    parent_vtxindex: 444,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 1,
                },
                res: Ok(())
            },
            CheckFixture {
                // accept -- builds directly off of genesis block and consumes leader_key_2
                op: LeaderBlockCommitOp {
                    block_header_hash: BlockHeaderHash::from_bytes(&hex_bytes("2222222222222222222222222222222222222222222222222222222222222222").unwrap()).unwrap(),
                    new_seed: VRFSeed::from_bytes(&hex_bytes("3333333333333333333333333333333333333333333333333333333333333333").unwrap()).unwrap(),
                    parent_block_backptr: 0,
                    parent_vtxindex: 0,
                    key_block_backptr: 2,
                    key_vtxindex: 457,
                    epoch_num: (126 - first_block_height) as u32,
                    memo: vec![0x80],

                    burn_fee: 12345,
                    input: BurnchainSigner {
                        public_keys: vec![
                            StacksPublicKey::from_hex("02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0").unwrap(),
                        ],
                        num_sigs: 1,
                        hash_mode: AddressHashMode::SerializeP2PKH
                    },

                    txid: Txid::from_bytes_be(&hex_bytes("3c07a0a93360bc85047bbaadd49e30c8af770f73a37e10fec400174d2e5f27cf").unwrap()).unwrap(),
                    vtxindex: 445,
                    block_height: 126,
                    burn_header_hash: block_126_hash.clone(),
                    fork_segment_id: 0,
                },
                res: Ok(())
            }
        ];

        for fixture in fixtures {
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

