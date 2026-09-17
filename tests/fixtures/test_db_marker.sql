-- Marker required before destructive test-suite resets.
CREATE SCHEMA qbot_test_guard;
CREATE TABLE qbot_test_guard.identity (
    marker text PRIMARY KEY
);
INSERT INTO qbot_test_guard.identity(marker) VALUES ('qbot-disposable-v1');
