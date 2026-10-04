-- Instance settings, string to string. Typing, defaults and validation live in
-- the server's Settings struct; values are only written through it.
CREATE TABLE setting (
    key text PRIMARY KEY,
    value text NOT NULL
);
