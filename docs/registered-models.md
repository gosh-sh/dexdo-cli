# Registered models on Acki Nacki Mainnet

The chain carries one canonical list of model names -- the on-chain ModelRegistry. Everything on this
page is read out of that list, so the list, and not a conversation, settles the two questions people
actually ask:

- **"Is model X supported?"** Export the list and look. A name that is not in it is not registered --
  see [Check whether one model is registered](#check-whether-one-model-is-registered).
- **"What exactly do I write when I sell it?"** The `frame_model` in your `models.json` must be one
  of these names, byte for byte -- see [Sellers: the name is the market](#sellers-the-name-is-the-market).

Being on the list is not the same as being on sale right now. That is the next section.

## Whether a model can be bought right now

Registration does not mean the model is currently available to buy. To see which markets currently
have bids and asks, use the market explorers:

- <https://markets.dex.do/#net=mainnet>
- <https://dex.acki.pro/>

## Export the canonical list

`dexdo model-registry` reads the ModelRegistry for the network selected by the current manifest and
writes a stable JSON snapshot. The command is read-only: it does not need a note or wallet and does
not submit a transaction.

```sh
dexdo model-registry --output model-registry.json
```

The command prints the registry address, network, model count and output path. The generated file
contains the `schema`, `network`, `registry`, `count` and `models` fields. Keep an old snapshot and
diff it against a new one to see what the registry gained or lost.

## Check whether one model is registered

```sh
MODEL='Jamba-tiny-random'

if jq -e --arg model "$MODEL" \
  '.models | index($model) != null' model-registry.json >/dev/null; then
  echo "registered"
else
  echo "not registered"
fi
```

Print every registered model name:

```sh
jq -r '.models[]' model-registry.json
```

Print the number of registered models:

```sh
jq '.count' model-registry.json
```

## Sellers: the name is the market

`frame_model` is not a label, it is the market's identity: the order book address is `sha256` over
its exact bytes. One changed character is a different address -- a market no buyer who took the name
from the registry can reach.

Registered spellings are producer-free and case-carrying: `Qwen3-32B`, not `Qwen/Qwen3-32B` and not
`qwen--qwen3--32b`. Copy the name out of the snapshot, and preserve spelling, case, punctuation,
hyphens and underscores when you set the `frame_model` field in `models.json` or pass the name as
`--frame-model`.
