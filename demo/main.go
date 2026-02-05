package main

import (
	"database/sql"
	"encoding/json"
	"log"
	"net/http"
	"strconv"

	_ "github.com/jackc/pgx/v5/stdlib"
)

var db *sql.DB

type User struct {
	ID        int    `json:"id,omitempty"`
	Name      string `json:"name"`
	Email     string `json:"email"`
	CreatedAt string `json:"created_at,omitempty"`
}

type Address struct {
	ID        int    `json:"id,omitempty"`
	UserID    int    `json:"user_id"`
	Street    string `json:"street"`
	City      string `json:"city"`
	Country   string `json:"country"`
	CreatedAt string `json:"created_at,omitempty"`
}

func main() {
	var err error
	db, err = sql.Open("pgx", "postgres://demo:demo@localhost:5432/demo?sslmode=disable")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	if err := db.Ping(); err != nil {
		log.Fatal(err)
	}

	http.HandleFunc("GET /health", healthHandler)
	http.HandleFunc("GET /users", listUsers)
	http.HandleFunc("POST /users", createUser)
	http.HandleFunc("GET /users/{id}", getUser)
	http.HandleFunc("GET /addresses", listAddresses)
	http.HandleFunc("POST /addresses", createAddress)
	http.HandleFunc("GET /addresses/{id}", getAddress)

	log.Println("Server listening on :8080")
	log.Fatal(http.ListenAndServe(":8080", nil))
}

func healthHandler(w http.ResponseWriter, r *http.Request) {
	w.WriteHeader(http.StatusOK)
	w.Write([]byte("ok"))
}

func listUsers(w http.ResponseWriter, r *http.Request) {
	rows, err := db.Query("SELECT id, name, email, created_at FROM users ORDER BY id")
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	var users []User
	for rows.Next() {
		var u User
		if err := rows.Scan(&u.ID, &u.Name, &u.Email, &u.CreatedAt); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		users = append(users, u)
	}
	json.NewEncoder(w).Encode(users)
}

func createUser(w http.ResponseWriter, r *http.Request) {
	var u User
	if err := json.NewDecoder(r.Body).Decode(&u); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	err := db.QueryRow(
		"INSERT INTO users (name, email) VALUES ($1, $2) RETURNING id, created_at",
		u.Name, u.Email,
	).Scan(&u.ID, &u.CreatedAt)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusCreated)
	json.NewEncoder(w).Encode(u)
}

func getUser(w http.ResponseWriter, r *http.Request) {
	id, err := strconv.Atoi(r.PathValue("id"))
	if err != nil {
		http.Error(w, "invalid id", http.StatusBadRequest)
		return
	}
	var u User
	err = db.QueryRow(
		"SELECT id, name, email, created_at FROM users WHERE id = $1", id,
	).Scan(&u.ID, &u.Name, &u.Email, &u.CreatedAt)
	if err == sql.ErrNoRows {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	json.NewEncoder(w).Encode(u)
}

func listAddresses(w http.ResponseWriter, r *http.Request) {
	rows, err := db.Query("SELECT id, user_id, street, city, country, created_at FROM addresses ORDER BY id")
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	defer rows.Close()

	var addresses []Address
	for rows.Next() {
		var a Address
		if err := rows.Scan(&a.ID, &a.UserID, &a.Street, &a.City, &a.Country, &a.CreatedAt); err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		addresses = append(addresses, a)
	}
	json.NewEncoder(w).Encode(addresses)
}

func createAddress(w http.ResponseWriter, r *http.Request) {
	var a Address
	if err := json.NewDecoder(r.Body).Decode(&a); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	err := db.QueryRow(
		"INSERT INTO addresses (user_id, street, city, country) VALUES ($1, $2, $3, $4) RETURNING id, created_at",
		a.UserID, a.Street, a.City, a.Country,
	).Scan(&a.ID, &a.CreatedAt)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.WriteHeader(http.StatusCreated)
	json.NewEncoder(w).Encode(a)
}

func getAddress(w http.ResponseWriter, r *http.Request) {
	id, err := strconv.Atoi(r.PathValue("id"))
	if err != nil {
		http.Error(w, "invalid id", http.StatusBadRequest)
		return
	}
	var a Address
	err = db.QueryRow(
		"SELECT id, user_id, street, city, country, created_at FROM addresses WHERE id = $1", id,
	).Scan(&a.ID, &a.UserID, &a.Street, &a.City, &a.Country, &a.CreatedAt)
	if err == sql.ErrNoRows {
		http.Error(w, "not found", http.StatusNotFound)
		return
	}
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	json.NewEncoder(w).Encode(a)
}
