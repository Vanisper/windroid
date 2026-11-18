package dev.windroid.fixture;

import android.app.Activity;
import android.content.SharedPreferences;
import android.os.Bundle;
import android.text.Editable;
import android.text.TextWatcher;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;

/** Both APKs use this activity. Android isolates its preferences by application ID. */
public final class MainActivity extends Activity implements TextWatcher {
    private int count;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        SharedPreferences state = getPreferences(MODE_PRIVATE);
        count = state.getInt("count", 0);

        LinearLayout layout = new LinearLayout(this);
        layout.setOrientation(LinearLayout.VERTICAL);
        int pad = Math.round(16 * getResources().getDisplayMetrics().density);
        layout.setPadding(pad, pad, pad, pad);

        EditText input = new EditText(this);
        input.setSingleLine(true);
        input.setHint("输入测试文本");
        input.setText(state.getString("text", ""));
        input.addTextChangedListener(this);
        layout.addView(input);

        Button button = new Button(this);
        button.setText("点击计数 " + count);
        button.setOnClickListener(view -> {
            count++;
            button.setText("点击计数 " + count);
            state.edit().putInt("count", count).apply();
        });
        layout.addView(button);
        setContentView(layout);
    }

    @Override public void beforeTextChanged(CharSequence s, int start, int count, int after) {}
    @Override public void onTextChanged(CharSequence s, int start, int before, int count) {}
    @Override public void afterTextChanged(Editable text) {
        getPreferences(MODE_PRIVATE).edit().putString("text", text.toString()).apply();
    }
}
