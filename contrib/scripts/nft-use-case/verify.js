import { uintCV, callReadOnlyFunction, cvToString } from "@stacks/transactions";
import { StacksTestnet, HIRO_MOCKNET_DEFAULT } from "@stacks/network";

async function main() {
  const network = new StacksTestnet({ url: HIRO_MOCKNET_DEFAULT });
  const senderAddress = process.env.ALT_USER_ADDR;
  const addr = process.env.USER_ADDR;

  const txOptions = {
    contractAddress: addr,
    contractName: "simple-nft-l1",
    functionName: "get-owner",
    functionArgs: [uintCV(5)],
    network,
    senderAddress,
  };

  const result = await callReadOnlyFunction(txOptions);

  console.log(cvToString(result.value));
}

main();
