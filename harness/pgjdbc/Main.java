import java.sql.*;
public class Main {
    public static void main(String[] args) throws Exception {
        DriverManager.setLoginTimeout(10);
        for (String key : new String[]{"PGFAULT_JDBC_DIRECT", "PGFAULT_JDBC_PROXY"}) {
            for (String mode : new String[]{"simple", "extended"}) {
                try (Connection c = DriverManager.getConnection(System.getenv(key) + "&preferQueryMode=" + mode + "&prepareThreshold=1")) {
                    try (PreparedStatement p = c.prepareStatement("SELECT ?::int")) {
                        for (int i=0;i<10;i++) { p.setInt(1,i); try (ResultSet r=p.executeQuery()) { if (!r.next() || r.getInt(1)!=i) throw new AssertionError("result mismatch"); } }
                    }
                    c.setAutoCommit(false);
                    try { c.createStatement().execute("SELECT 1/0"); throw new AssertionError("expected error"); } catch (SQLException expected) { if (!"22012".equals(expected.getSQLState())) throw expected; }
                    c.rollback();
                    try (ResultSet r=c.createStatement().executeQuery("SELECT 42")) { if (!r.next() || r.getInt(1)!=42) throw new AssertionError("recovery failed"); }
                    c.rollback();
                }
            }
        }
        String fault=System.getenv("PGFAULT_JDBC_FAULT");
        if (fault!=null) {
            try (Connection oracle=DriverManager.getConnection(System.getenv("PGFAULT_JDBC_DIRECT"))) {
                String table="pgfault_java_"+System.nanoTime();
                oracle.createStatement().execute("CREATE TABLE "+table+" (id int)");
                try {
                    try (Connection c=DriverManager.getConnection(fault)) {
                        c.setAutoCommit(false); c.createStatement().execute("INSERT INTO "+table+" VALUES (1)");
                        try { c.commit(); throw new AssertionError("commit unexpectedly succeeded"); } catch (SQLException expected) { if (expected.getSQLState()==null || !expected.getSQLState().startsWith("08")) throw expected; }
                    }
                    try (ResultSet r=oracle.createStatement().executeQuery("SELECT count(*) FROM "+table)) { if (!r.next() || r.getInt(1)!=1) throw new AssertionError("durable effect missing"); }
                } finally { oracle.createStatement().execute("DROP TABLE "+table); }
            }
        }
        System.out.println("{\"driver\":\"pgjdbc\",\"transparency\":\"passed\",\"ambiguous_commit\":\"passed if PGFAULT_JDBC_FAULT set\"}");
    }
}
