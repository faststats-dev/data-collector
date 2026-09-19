package example;

public class Main {
    public static void main(String[] args) {
        try {
            new Worker().run(args.length);
        } catch (RuntimeException error) {
            error.printStackTrace();
        }
    }
}
