;; The withdraw contract
;; For withdrawing assets from a subnmet back to the L1.

(define-trait nft-trait
  (
    ;; Last token ID, limited to uint range
    (get-last-token-id () (response uint uint))

    ;; URI for metadata associated with the token
    (get-token-uri (uint) (response (optional (string-ascii 256)) uint))

     ;; Owner of a given token identifier
    (get-owner (uint) (response (optional principal) uint))

    ;; Transfer from the sender to a new principal
    (transfer (uint principal principal) (response bool uint))
  )
)

(define-trait ft-trait
  (
    ;; Transfer from the caller to a new principal
    (transfer (uint principal principal (optional (buff 34))) (response bool uint))

    ;; the human readable name of the token
    (get-name () (response (string-ascii 32) uint))

    ;; the ticker symbol, or empty if none
    (get-symbol () (response (string-ascii 32) uint))

    ;; the number of decimals used, e.g. 6 would mean 1_000_000 represents 1 token
    (get-decimals () (response uint uint))

    ;; the balance of the passed principal
    (get-balance (principal) (response uint uint))

    ;; the current total supply (which does not need to be a constant)
    (get-total-supply () (response uint uint))

    ;; an optional URI that represents metadata of this token
    (get-token-uri () (response (optional (string-utf8 256)) uint))
  )
)

(define-trait subnet-asset
  (
    ;; Process a deposit from the burnchain.
    (deposit-from-burnchain
      (
        uint       ;; asset-id (NFT) or amount (FT)
        principal  ;; recipient
      )
      (response bool uint)
    )

    ;; Burn the asset for withdrawal from the subnet.
    (burn-for-withdrawal
      (
        uint       ;; asset-id (NFT) or amount (FT)
        principal  ;; owner
      )
      (response bool uint)
    )
  )
)

(define-public (ft-withdraw? (asset <subnet-asset>) (amount uint) (sender principal))
    (begin
        (print {
            type: "ft",
            sender: sender,
            amount: amount,
            asset-contract: (contract-of asset),
            withdrawal-height: block-height,
        })
        (try! (contract-call? asset burn-for-withdrawal amount sender))
        (ok block-height)
    )
)

(define-public (nft-withdraw? (asset <subnet-asset>) (id uint) (sender principal))
    (begin
        (print {
            type: "nft",
            sender: sender,
            id: id,
            asset-contract: (contract-of asset),
            withdrawal-height: block-height,
        })
        (try! (contract-call? asset burn-for-withdrawal id sender))
        (ok block-height)
    )
)

(define-public (stx-withdraw? (amount uint) (sender principal))
    (begin
        (print {
            type: "stx",
            sender: sender,
            amount: amount,
            withdrawal-height: block-height,
        })
        (try! (stx-transfer? amount sender (as-contract tx-sender)))
        (ok block-height)
    )
)
