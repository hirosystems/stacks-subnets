import {
    makeContractCall,
    AnchorMode,
    contractPrincipalCV,
    stringAsciiCV,
    broadcastTransaction,
} from '@stacks/transactions';
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from '@stacks/network';


async function main() {
    const network = new StacksTestnet({url: HIRO_MOCKNET_DEFAULT});
    const senderKey = process.env.AUTH_SUBNET_MINER_KEY;
    const userAddr = process.env.USER_ADDR;
    const nonce = parseInt(process.argv[2]);

    const txOptions = {
        contractAddress: 'ST1PQHQKV0RJXZFY1DGX8MNSNYVE3VGZJSRTPGZGM',
        contractName: 'subnet-alpha',
        functionName: 'register-new-nft-contract',
        functionArgs: [contractPrincipalCV(userAddr, 'simple-nft-l1'), stringAsciiCV("subnet-deposit-nft-token")],
        senderKey,
        validateWithAbi: false,
        network,
        anchorMode: AnchorMode.Any,
        fee: 10000,
        nonce,
    };

    const transaction = await makeContractCall(txOptions);

    const txid = await broadcastTransaction(
        transaction, network
    );

    console.log(txid);
}

main()