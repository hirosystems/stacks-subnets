;; The .multi-miner contract
(define-constant CONTRACT_ADDRESS (as-contract tx-sender))

(define-constant ERR_SIGNER_APPEARS_TWICE 101)
(define-constant ERR_NOT_ENOUGH_SIGNERS 102)
(define-constant ERR_INVALID_SIGNATURE 103)
(define-constant ERR_UNAUTHORIZED_CONTRACT_CALLER 104)
(define-constant ERR_MINER_ALREADY_SET 105)
(define-constant ERR_UNSUPPORTED_SUBNET_CONTRACT_VERSION 106)

;; SIP-018 Constants
(define-constant sip18-prefix 0x534950303138)
;; (define-constant sip18-domain-hash
;;     (sha256 (unwrap-panic (to-consensus-buff? {
;;         name: "subnet-multi-miner",
;;         version: "1.0.0",
;;         chain-id: u1
;;     }))))
(define-constant sip18-domain-hash 0x81c24181e24119f609a28023c4943d3a41592656eb90560c15ee02b8e1ce19b8)
(define-constant sip18-data-prefix (concat sip18-prefix sip18-domain-hash))

;; Use trait declarations
(use-trait nft-trait 'SP2PABAF9FTAJYNFZH93XENAJ8FVY99RRM50D2JG9.nft-trait.nft-trait)
(use-trait ft-trait 'SP3FBR2AGK5H9QBDH3EEN6DF8EK8JY7RX8QJ5SVTE.sip-010-trait-ft-standard.sip-010-trait)
(use-trait mint-from-subnet-trait .subnet-traits.mint-from-subnet-trait)

;; Required number of signers
(define-constant signers-required u2)

;; List of miners
(define-data-var miners (optional (list 10 principal)) none)

;; Minimun version of subnet contract required
(define-constant SUBNET_CONTRACT_VERSION_MIN {
    major: u3,
    minor: u0,
    patch: u0,
})

;; Return error if subnet contract version not supported
(define-read-only (check-subnet-contract-version) (
    let (
        (subnet-contract-version (contract-call? .subnet-v3-0-0 get-version))
    )

    ;; Check subnet contract version is greater than min supported version
    (asserts! (is-eq (get major subnet-contract-version) (get major SUBNET_CONTRACT_VERSION_MIN)) (err ERR_UNSUPPORTED_SUBNET_CONTRACT_VERSION))
    (asserts! (>= (get minor subnet-contract-version) (get minor SUBNET_CONTRACT_VERSION_MIN)) (err ERR_UNSUPPORTED_SUBNET_CONTRACT_VERSION))
    ;; Only check patch version if major and minor version are equal
    (asserts! (or
            (not (is-eq (get minor subnet-contract-version) (get minor SUBNET_CONTRACT_VERSION_MIN)))
            (>= (get patch subnet-contract-version) (get patch SUBNET_CONTRACT_VERSION_MIN)))
        (err ERR_UNSUPPORTED_SUBNET_CONTRACT_VERSION))
    (ok true)
))

;; Fail if the subnet contract is not compatible
(try! (check-subnet-contract-version))

(define-private (get-miners)
    (unwrap-panic (var-get miners)))

;; Set the miners for this contract. Can be called by *anyone*
;;  before the miner is set. This is an unsafe way to initialize the
;;  contract, because a re-org could allow someone to reinitialize
;;  this field. Instead, authors should initialize the variable
;;  directly at the data-var instantiation. This is used for testing
;;  purposes only. 
(define-public (set-miners (miners-to-set (list 10 principal)))
    (match (var-get miners) existing-miner (err ERR_MINER_ALREADY_SET) 
        (begin 
            (var-set miners (some miners-to-set))
            (ok true))))

(define-private (index-of-miner (to-check principal))
    (index-of (get-miners) to-check))

(define-private (test-is-none (to-check (optional uint)))
    (is-some to-check))

(define-private (unique-helper (item (optional uint)) (accum { all-unique: bool,  priors: (list 10 uint)}))
    (if (not (get all-unique accum))
        { all-unique: false, priors: (list) }
        (if (is-some (index-of (get priors accum) (unwrap-panic item)))
            { all-unique: false, priors: (list) }
            { all-unique: true,
              priors: (unwrap-panic (as-max-len? (append (get priors accum) (unwrap-panic item)) u10)) })))

(define-private (check-miners (provided-set (list 10 principal)))
    (let ((provided-checked (filter test-is-none (map index-of-miner provided-set)))
          (uniques-checked (fold unique-helper provided-checked { all-unique: true, priors: (list)})))
         (asserts! (get all-unique uniques-checked) (err ERR_SIGNER_APPEARS_TWICE))
         (asserts! (>= (len provided-checked) signers-required) (err ERR_NOT_ENOUGH_SIGNERS))
         (ok true)))

(define-read-only (make-block-commit-hash (block-data { block: (buff 32), subnet-block-height: uint, withdrawal-root: (buff 32), target-tip: (buff 32), target-height: uint }))
    (let ((data-buff (unwrap-panic (to-consensus-buff? (merge block-data { multi-contract: CONTRACT_ADDRESS }))))
          (data-hash (sha256 data-buff))
        (structured-hash (sha256 (concat sip18-data-prefix data-hash))))
        structured-hash
    )
)

(define-private (verify-sign-helper (curr-signature (buff 65))
                                    (accum (response { hash: (buff 32), signers: (list 9 principal) } int)))
    (match accum
        prior-okay (let ((curr-signer-pk (unwrap! (secp256k1-recover? (get hash prior-okay) curr-signature)
                                                (err ERR_INVALID_SIGNATURE)))
                         (curr-signer (unwrap! (principal-of? curr-signer-pk) (err ERR_INVALID_SIGNATURE))))
                        (ok { hash: (get hash prior-okay),
                              signers: (unwrap-panic (as-max-len? (append (get signers prior-okay) curr-signer) u9)) }))
        prior-err (err prior-err)))

(define-public (commit-block
        (block-data { block: (buff 32), subnet-block-height: uint, withdrawal-root: (buff 32), target-tip: (buff 32), target-height: uint })
        (signatures (list 9 (buff 65)))
    )
    (let ((block-data-hash (make-block-commit-hash block-data))
          (signer-principals (try! (fold verify-sign-helper signatures (ok { hash: block-data-hash, signers: (list) })))))
         ;; check that the caller is a direct caller!
         (asserts! (is-eq tx-sender contract-caller) (err ERR_UNAUTHORIZED_CONTRACT_CALLER))
         ;; check that we have enough signatures
         (try! (check-miners (append (get signers signer-principals) tx-sender)))
         ;; execute the block commit
         (as-contract
            (contract-call?
                .subnet-v3-0-0
                commit-block
                (get block block-data)
                (get subnet-block-height block-data)
                (get target-tip block-data)
                (get target-height block-data)
                (get withdrawal-root block-data)
            )
        )
    )
)

;; miner needs to pass in the block height at the time the proposal was created
;; the id-header-hash for that block height (on the current fork) will verify that the signatures
;; are for the function by that name on this fork
(define-private (check-registration
        (signatures (list 9 (buff 65)))
        (data {
            l1-contract: principal,
            l2-contract: principal,
            height: uint
        })
    )
    (let ((registration-hash (make-registration-hash data))
          (signer-principals (try! (fold verify-sign-helper signatures (ok { hash: registration-hash, signers: (list) }))))
        )
        ;; TODO: perform checks on height?
        ;; TODO: should we pass around the block-id as well to provide a meaningful error?
        ;; check that the caller is a direct caller!
        (asserts! (is-eq tx-sender contract-caller) (err ERR_UNAUTHORIZED_CONTRACT_CALLER))
        ;; check that we have enough signatures
        (check-miners (append (get signers signer-principals) tx-sender))
    )
)

;; TODO: this needs to be ensure that the miner can't call it directly with an earlier height
;; so it either needs to be private or we could check that the height is recent
(define-private (make-registration-hash
        (data {
            l1-contract: principal,
            l2-contract: principal,
            height: uint
        })
    )
    (let (
            (block-id (get-block-info? id-header-hash (get height data)))
            (data-buff (unwrap-panic (to-consensus-buff? (merge data { block-id: block-id, multi-contract: CONTRACT_ADDRESS }))))
            (data-hash (sha256 data-buff))
            (structured-hash (sha256 (concat sip18-data-prefix data-hash)))
        )
        structured-hash
    )
)

(define-read-only (make-ft-registration-hash (ft-contract <ft-trait>) (l2-contract principal))
    (let (
            (contract_principal (contract-of ft-contract))
            (structured-hash (make-registration-hash
                {
                    l1-contract: (contract-of ft-contract),
                    l2-contract: l2-contract,
                    height: block-height
                }))
        )
        { height: block-height, hash: structured-hash }
    )
)

(define-read-only (make-nft-registration-hash (nft-contract <nft-trait>) (l2-contract principal))
    (let (
            (contract_principal (contract-of nft-contract))
            (structured-hash (make-registration-hash
                {
                    l1-contract: (contract-of nft-contract),
                    l2-contract: l2-contract,
                    height: block-height
                }))
        )
        { height: block-height, hash: structured-hash }
    )
)

;; height is the block-height when the hash was created that was signed
;; the purpose of this is to ensure that this is the same fork
;; nft-contract is on the L1
;; l2-contract is on the L2
(define-public (register-new-ft-contract
        (ft-contract <ft-trait>)
        (l2-contract principal)
        (height uint)
        (signatures (list 9 (buff 65)))
    )
    (begin
        (try! (check-registration signatures
            {
                l1-contract: (contract-of ft-contract),
                l2-contract: l2-contract,
                height: height
            }
        ))
        ;; execute the registration
        (as-contract (contract-call? .subnet register-new-ft-contract ft-contract l2-contract))
    )
)

;; height is the block-height when the hash was created that was signed
;; the purpose of this is to ensure that this is the same fork
;; nft-contract is on the L1
;; l2-contract is on the L2
(define-public (register-new-nft-contract
        (nft-contract <nft-trait>)
        (l2-contract principal)
        (height uint)
        (signatures (list 9 (buff 65)))
    )
    (begin
        (try! (check-registration signatures
            {
                l1-contract: (contract-of nft-contract),
                l2-contract: l2-contract,
                height: height
            }
        ))
        ;; execute the registration
        (as-contract (contract-call? .subnet register-new-nft-contract nft-contract l2-contract))
    )
)
