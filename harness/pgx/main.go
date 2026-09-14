// Differential transparency and ambiguous commit with pgx. No retry loop.
package main

import (
    "context"
    "fmt"
    "os"
    "time"
    "github.com/jackc/pgx/v5"
)
func check(err error) { if err != nil { panic(err) } }
func main() {
    ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second); defer cancel()
    direct := os.Getenv("PGFAULT_DIRECT")
    proxy := os.Getenv("PGFAULT_PROXY")
    for _, url := range []string{direct, proxy} {
        c, err := pgx.Connect(ctx, url); check(err)
        for _, mode := range []pgx.QueryExecMode{pgx.QueryExecModeSimpleProtocol,pgx.QueryExecModeExec,pgx.QueryExecModeCacheStatement} {
            for i:=0;i<10;i++ { var n int; check(c.QueryRow(ctx,"SELECT $1::int",mode,i).Scan(&n)); if n!=i {panic("wrong result")} }
        }
        tx, err:=c.Begin(ctx);check(err)
        _,err=tx.Exec(ctx,"SELECT 1/0");if err==nil {panic("missing query failure")}
        check(tx.Rollback(ctx));var n int;check(c.QueryRow(ctx,"SELECT 42").Scan(&n));if n!=42 {panic("recovery failed")}
        check(c.Close(ctx))
    }
    if fault:=os.Getenv("PGFAULT_FAULT");fault!="" {
        oracle,err:=pgx.Connect(ctx,direct);check(err);defer oracle.Close(ctx)
        table:=fmt.Sprintf("pgfault_go_%d",time.Now().UnixNano())
        _,err=oracle.Exec(ctx,"CREATE TABLE "+table+" (id int)");check(err)
        defer oracle.Exec(ctx,"DROP TABLE "+table)
        c,err:=pgx.Connect(ctx,fault);check(err);defer c.Close(ctx)
        tx,err:=c.Begin(ctx);check(err);_,err=tx.Exec(ctx,"INSERT INTO "+table+" VALUES (1)");check(err)
        if err=tx.Commit(ctx);err==nil {panic("COMMIT unexpectedly succeeded")}
        var n int;check(oracle.QueryRow(ctx,"SELECT count(*) FROM "+table).Scan(&n));if n!=1 {panic("durable row missing")}
    }
    fmt.Println(`{"driver":"pgx","transparency":"passed","ambiguous_commit":"passed if PGFAULT_FAULT set"}`)
}
