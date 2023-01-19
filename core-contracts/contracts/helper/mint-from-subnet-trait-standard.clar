(define-trait mint-from-subnet-trait
  (
    ;; Transfer from the sender to a new principal
    (mint-from-subnet (uint principal principal) (response bool uint))
  )
)