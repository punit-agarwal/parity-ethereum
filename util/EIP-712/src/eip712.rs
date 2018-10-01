// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! EIP712 structs
//!

use serde_json::{Value};
use std::collections::HashMap;
use ethereum_types::{U256, H256, Address};

pub(crate) type MessageTypes = HashMap<String, Vec<FieldType>>;


#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub(crate) struct EIP712Domain {
	pub(crate) name: String,
	pub(crate) version: String,
	pub(crate) chain_id: U256,
	pub(crate) verifying_contract: Address,
	#[serde(skip_serializing_if="Option::is_none")]
	pub(crate) salt: Option<H256>,
}
/// EIP-712 struct
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[derive(Deserialize, Debug, Clone)]
pub struct EIP712 {
	pub(crate) types: MessageTypes,
	pub(crate) primary_type: String,
	pub(crate) message: Value,
	pub(crate) domain: EIP712Domain,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct FieldType {
	pub name: String,
	#[serde(rename = "type")]
	pub type_: String,
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::from_str;
	#[test]
	fn test_deserialization() {
		let string = r#"{
            "primaryType": "Mail",
			"domain": {
				"name": "Ether Mail",
				"version": "1",
				"chainId": "0x1",
				"verifyingContract": "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
			},
			"message": {
				"from": {
					"name": "Cow",
					"wallet": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826"
				},
				"to": {
					"name": "Bob",
					"wallet": "0xbBbBBBBbbBBBbbbBbbBbbbbBBbBbbbbBbBbbBBbB"
				},
				"contents": "Hello, Bob!"
			},
			"types": {
				"EIP712Domain": [
				    { "name": "name", "type": "string" },
					{ "name": "version", "type": "string" },
					{ "name": "chainId", "type": "uint256" },
					{ "name": "verifyingContract", "type": "address" }
				],
				"Person": [
					{ "name": "name", "type": "string" },
					{ "name": "wallet", "type": "address" }
				],
				"Mail": [
					{ "name": "from", "type": "Person" },
					{ "name": "to", "type": "Person" },
					{ "name": "contents", "type": "string" }
				]
			}
        }"#;
		let _ = from_str::<EIP712>(string).unwrap();
	}
}