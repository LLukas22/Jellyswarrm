ALTER TABLE server_mappings ADD COLUMN key_password_rejected BOOLEAN NOT NULL DEFAULT FALSE;

CREATE TRIGGER reset_key_password_rejection
AFTER UPDATE OF mapped_username, mapped_password, auth_method ON server_mappings
WHEN OLD.key_password_rejected AND (
    NEW.mapped_username <> OLD.mapped_username OR
    NEW.mapped_password <> OLD.mapped_password OR
    NEW.auth_method <> OLD.auth_method
)
BEGIN
    UPDATE server_mappings SET key_password_rejected = FALSE WHERE id = NEW.id;
END;
