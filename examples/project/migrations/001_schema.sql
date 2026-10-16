CREATE TABLE users (
    id INT PRIMARY KEY
);

CREATE TABLE posts (
    id INT PRIMARY KEY,
    user_id INT,
    FOREIGN KEY (user_id) REFERENCES users(id)
);
