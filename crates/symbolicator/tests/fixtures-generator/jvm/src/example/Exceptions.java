package example;

public class Exceptions {
    public static void main(String[] args) {
        try {
            new Task().run(3);
        } catch (RuntimeException cause) {
            Failure failure = new Failure("outer", cause);
            failure.addSuppressed(new Failure("suppressed", null));
            Thread thread = Thread.currentThread();
            thread.getUncaughtExceptionHandler().uncaughtException(thread, failure);
        }
    }

    static class Task {
        void run(int depth) {
            if (depth > 0) {
                run(depth - 1);
            } else {
                run("nested failure");
            }
        }

        void run(String message) {
            throw new Failure(message, null);
        }
    }

    static class Failure extends RuntimeException {
        Failure(String message, Throwable cause) {
            super(message, cause);
        }
    }
}
