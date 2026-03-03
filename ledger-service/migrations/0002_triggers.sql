CREATE OR REPLACE FUNCTION set_updated_at()
RETURNS TRIGGER AS $$
BEGIN
  NEW.updated_at = NOW();
  RETURN NEW;
END;
$$ language 'plpgsql';

DROP TRIGGER IF EXISTS wallets_updated_at_trigger ON wallets;
CREATE TRIGGER wallets_updated_at_trigger
  BEFORE UPDATE ON wallets
  FOR EACH ROW
  EXECUTE PROCEDURE set_updated_at();

DROP TRIGGER IF EXISTS reservations_updated_at_trigger ON reservations;
CREATE TRIGGER reservations_updated_at_trigger
  BEFORE UPDATE ON reservations
  FOR EACH ROW
  EXECUTE PROCEDURE set_updated_at();
