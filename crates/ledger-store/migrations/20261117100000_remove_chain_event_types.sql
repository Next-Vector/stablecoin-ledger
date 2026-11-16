-- Deposits, withdrawals and reconciliation moved to the chain module, so the ledger can never emit
-- their events (#66). A subscription to nothing but those could never receive anything, so it is
-- deleted (its deliveries go with it); the others lose just those types. No event of these types
-- was ever written, so the outbox needs no change.
DELETE FROM webhook_endpoints
WHERE event_types <@ ARRAY['deposit.seen', 'deposit.confirmed', 'deposit.orphaned',
                           'withdrawal.broadcast', 'withdrawal.confirmed', 'withdrawal.failed',
                           'reconciliation.mismatch'];

UPDATE webhook_endpoints
SET event_types = ARRAY(
    SELECT t FROM unnest(event_types) WITH ORDINALITY AS e(t, i)
    WHERE t NOT IN ('deposit.seen', 'deposit.confirmed', 'deposit.orphaned',
                    'withdrawal.broadcast', 'withdrawal.confirmed', 'withdrawal.failed',
                    'reconciliation.mismatch')
    ORDER BY i
)
WHERE event_types && ARRAY['deposit.seen', 'deposit.confirmed', 'deposit.orphaned',
                           'withdrawal.broadcast', 'withdrawal.confirmed', 'withdrawal.failed',
                           'reconciliation.mismatch'];
