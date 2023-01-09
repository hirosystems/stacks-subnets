;; The withdraw contract
;; For withdrawing assets from a subnmet back to the L1.

(use-trait ft-trait 'SP3FBR2AGK5H9QBDH3EEN6DF8EK8JY7RX8QJ5SVTE.sip-010-trait-ft-standard)
(use-trait nft-trait 'SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.nft-trait.nft-trait)

(define-map withdraw { key-name-1: key-type-1 } { val-name-1: vals-type-1 })

(define-public (ft-withdraw? (asset <ft-trait>) (amount uint) (sender principal))
    (begin
        (print {
            "type": "ft",
            "sender": sender,
            "amount": amount,
            "asset-contract": (contract-of asset),
        })
        (contract-call? asset transfer amount tx-sender (as-contract tx-sender))
    )
)

(define-public (nft-withdraw? (asset <nft-trait>) (id uint) (sender principal))
    (begin
        (print {
            "type": "nft",
            "sender": sender,
            "id": id,
            "asset-contract": (contract-of asset),
        })
        (contract-call? asset transfer id tx-sender (as-contract tx-sender))
    )
)

(define-public (stx-withdraw? (amount uint) (sender principal))
    (begin
        (print {
            "type": "stx",
            "sender": sender,
            "amount": amount,
        })
        (stx-transfer? amount tx-sender (as-contract tx-sender))
    )
)
